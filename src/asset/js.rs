//! `asset.js` — bundle a JavaScript/TypeScript/JSX entry module, and
//! everything it imports, with [rolldown](https://rolldown.rs).
//!
//! rolldown is used as a Rust library, under the same guarantees as every
//! other twyla asset:
//!
//! - **No JS ever runs.** No node, no JS config files, no JS plugins; the
//!   only plugin is [`TwylaPlugin`] (Rust). Everything rolldown would read
//!   from a config file is an `asset.js` field.
//! - **Every file access goes through [`SandboxFs`]**, which confines reads
//!   to the project root (anything else is `NotFound`, which the resolver
//!   treats as "keep looking"), refuses every write, and records each read,
//!   probe and directory listing in the dep [ledger](crate::ledger) — so
//!   editing an imported module, a `package.json` or a `tsconfig.json`, or
//!   creating a file an import would now resolve to, invalidates the asset,
//!   whether or not the build succeeded.
//! - Two rolldown paths that would read the disk *behind* the sandbox are
//!   closed off: tsconfig discovery for the transform (rolldown's "raw"
//!   transform mode resolves it with its own OS-backed resolver, so twyla
//!   finds the entry's tsconfig through the sandbox and hands the merged
//!   options over instead — see [`build_group`]), and asset emission
//!   (`new URL(.., import.meta.url)` / asset module types, read with
//!   `tokio::fs`), which [`TwylaPlugin`] refuses along with every other
//!   non-JS import.
//!
//! **Naming.** rolldown picks the file names (`[name]-[hash].js`) and writes
//! them into the code as relative imports (`import "./chunk-x.js"`), so the
//! entry and its chunks are emitted at rolldown's names, flat under
//! `assets/` ([`Built::output_path`], [`Built::companions`]); twyla doesn't
//! re-fingerprint them. The hash is rolldown's content hash, so one name
//! always means the same bytes, and two builds sharing a chunk write it once.
//!
//! **Groups.** Every `asset.js` requested in a compile with equal
//! [`JsOptions`] is bundled by one [`build_group`] call — one rolldown build
//! — so modules they share become shared chunks, emitted once. Without code
//! splitting (`split: false`, `iife`) each entry is a group of its own. The
//! resolver forms the groups and caches them
//! ([`Resolver::prebuild_js_groups`](crate::resolver::Resolver::prebuild_js_groups)).

use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use arcstr::ArcStr;
use ecow::{EcoString, eco_format};
use oxc_resolver::{FileMetadata, FileSystem as OxcFileSystem, ResolveError};
use rolldown::{
    BundleFactory, BundleFactoryOptions, BundlerOptions, BundlerTransformOptions,
    CodeSplittingMode, Either, InjectImport, InputItem, IsExternal, ModuleType, OutputFormat,
    RawMinifyOptions, ResolveOptions, SourceMapType, TsConfig, merge_tsconfig,
};
use rolldown_common::Output;
use rolldown_error::{BuildDiagnostic, DiagnosticOptions, EventKind};
use rolldown_fs::OsFileSystem;
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookUsage, Plugin, PluginContext, SharedLoadPluginContext,
};
use typst::diag::{HintedStrResult, HintedString};
use typst::foundations::{
    Array, Bytes, Cast, Content, Dict, Packed, ShowFn, Smart, Str, StyleChain, Value, cast, elem,
    func, scope,
};
use typst::loading::{Encoding, Readable};
use typst::syntax::VirtualRoot;

use super::{
    AssetInput, AssetSpec, Built, Companion, OutputReq, decode, decode_empty, emit_or_request,
    introspect_asset, resolve_or_request, resolve_path, sha256,
};
use crate::ledger::DepRecorder;
use crate::project::TwylaContext;
use crate::render::Emit;

/// Bundle a JavaScript, TypeScript or JSX entry module — and everything it
/// imports — into fingerprinted JS files with
/// #link("https://rolldown.rs")[rolldown].
///
/// Imports resolve the way a bundler resolves them: relative paths, packages
/// in `node_modules` (`package.json` `exports`, with the platform's
/// conditions), and `tsconfig.json` `paths` aliases. TypeScript and JSX are
/// compiled using the `tsconfig.json` that governs the entry (its `jsx`,
/// `jsxImportSource`, `jsxFactory`, decorator options, …), applied to every
/// module in the bundle. Install packages with your package manager as
/// usual; twyla only reads `node_modules`, it never runs anything.
///
/// Every file the bundle reads must be inside the project: imports that
/// reach outside it (including through a symlink) fail to resolve. Only
/// JavaScript, TypeScript, JSON and text modules can be imported — not CSS
/// or images (use the other `asset` types for those).
///
/// With code splitting (the default for `"esm"`), every `asset.js` in the
/// site with identical options (all fields but `path`) is bundled together,
/// in one build, whichever pages they're on: a module two entries import
/// becomes one shared chunk, emitted once, so a browser that loaded it for
/// one page has it cached for the next. (The exception: entries governed
/// by different `tsconfig.json` files are bundled separately, since one
/// tsconfig applies to a whole bundle, and share no chunks.) Modules loaded
/// with dynamic `import()` become chunks too. The chunks are separate files
/// next to the entry, emitted along with it. Use
/// #link("#asset-js-preload")[`.preload()`] to list the chunks the entry
/// needs right away (which includes any it shares with other entries):
///
/// ```typ
/// #let app = asset.js(
///   "/scripts/main.tsx",
///   define: (SITE: (debug: false)),
///   modules: ("virtual:posts": posts),
/// )
/// #context {
///   for u in app.preload() { html.link(rel: "modulepreload", href: u) }
///   html.script(type: "module", src: app.url())
/// }
/// ```
///
/// `.read()` inlines the entry's code (for an inline `<script>`) only when it
/// has no chunks: an inline script can't resolve their relative URLs. Use
/// `split: false` to bundle everything into one file first (an entry with
/// code splitting can gain shared chunks when another page adds an entry
/// that imports the same modules).
///
/// Unlike the other asset types, `asset.js` has no `output` field: rolldown
/// names the entry and its chunks (`main-<hash>.js`) and the entry imports
/// its chunks by those relative names, so it can't be moved elsewhere. All
/// the files land flat in `assets/`.
///
/// Like every bundler targeting browsers, `process.env.NODE_ENV` is replaced
/// on the `"browser"` platform: with `"production"` when `minify` is on and
/// `"development"` when it's off (libraries like Preact and React key their
/// debug code off it). Override it with
/// `define: ("process.env.NODE_ENV": "production")`. Other platforms leave
/// it alone.
#[elem(scope, name = "js")]
pub struct JsAsset {
    /// The entry module (`.js`, `.mjs`, `.ts`, `.tsx`, `.jsx`, …), relative to
    /// the calling file. Must be a path, not bytes: imports resolve relative
    /// to the entry's location.
    #[required]
    pub path: AssetInput,

    /// Minify the output (whitespace, names, dead code). Part of the asset
    /// key, so the minified and expanded builds of one entry resolve to
    /// distinct assets.
    #[default(true)]
    pub minify: bool,

    /// Emit a sourcemap (`<chunk>.sm.js.map`) next to every emitted JS file,
    /// referenced from it by a `//# sourceMappingURL` comment. (Files built
    /// with sourcemaps are named `<name>-<hash>.sm.js`, apart from the plain
    /// build's.) `.read()` drops the comment.
    #[default(false)]
    pub sourcemap: bool,

    /// The oldest JavaScript version to support, e.g. `"es2020"`: newer
    /// syntax is lowered to it. `{none}` leaves syntax as written.
    pub target: Option<EcoString>,

    /// The output format: `"esm"` (an ES module, for
    /// `<script type="module">`) or `"iife"` (a classic script wrapped in a
    /// function, for a plain `<script>`; it can't load chunks, so it implies
    /// `split: false`, and the entry's exports aren't reachable from outside).
    #[default(Format::Esm)]
    pub format: Format,

    /// Code splitting. When on, the entry is bundled together with every
    /// other `asset.js` with the same options, and every dynamic `import()`
    /// and every module shared between entries or chunks becomes its own
    /// chunk file, emitted with the entry. When off, the entry is bundled on
    /// its own, and everything — dynamic imports included — is inlined into
    /// the one entry file. `{auto}` means on for `"esm"` and off
    /// for `"iife"`; `split: true` with `format: "iife"` is an error.
    pub split: Smart<bool>,

    /// The platform to bundle for: `"browser"`, `"node"` or `"neutral"`.
    /// Decides which `package.json` conditions and fields are used to
    /// resolve packages (`browser`/`module`/`main` for the browser, none
    /// but `exports` for neutral) and whether `process.env.NODE_ENV` is
    /// replaced.
    #[default(Platform::Browser)]
    pub platform: Platform,

    /// Import specifiers to leave as imports in the output instead of
    /// bundling, e.g. a package loaded from an import map at runtime. Any
    /// other import that can't be resolved is an error. Some imports are
    /// always left alone without being listed: `http://`, `https://` and
    /// `//` URLs (a CDN), and on the `"node"` platform Node's builtin modules
    /// (`fs`, `node:path`, …).
    pub external: Vec<EcoString>,

    /// Global identifiers to replace with constants at build time. A nested
    /// dictionary defines each leaf by its dotted path —
    /// `(SITE: (debug: false))` replaces `SITE.debug` with `false` — so
    /// branches on it are removed by minification. Leaves are encoded as
    /// JSON: strings become string literals, arrays array literals.
    pub define: Dict,

    /// Virtual modules: each key is an import specifier, and importing it
    /// gives the value, as a JSON module. Passes typst data to scripts:
    /// `modules: ("virtual:posts": posts)` makes
    /// `import posts from "virtual:posts"` work.
    pub modules: Dict,

    /// Globals to supply by import: each key is a global name the code uses
    /// freely, and the value is the module to import it from — a specifier
    /// string (its default export) or a `(specifier, export)` array. An
    /// import is added only to modules that use the name. Specifiers are
    /// JavaScript ones, resolved like an import written in the module that
    /// uses the global: a package name, a `virtual:` module, or a path
    /// relative to that module (`/…` is a filesystem path, not the project
    /// root).
    pub inject: Dict,
}

#[scope]
impl JsAsset {
    /// The resolved URL of the entry file. Its chunks are emitted with it.
    #[func(contextual)]
    fn url(
        engine: &mut ::typst::engine::Engine,
        context: ::comemo::Tracked<::typst::foundations::Context>,
        this: Content,
    ) -> HintedStrResult<Str> {
        let elem = this.into_packed::<JsAsset>().unwrap();
        let spec = spec(&elem, context.styles()?)?;
        Ok(resolve_or_request(
            engine,
            spec,
            elem.span(),
            OutputReq::Auto,
        ))
    }

    /// The entry file's code, to inline in a `<script>` instead of linking
    /// it. Errors if the entry imports chunks, since an inline script can't
    /// resolve their relative URLs: use `.url()`, or `split: false` to
    /// bundle everything into the one file. With `sourcemap: true`, the
    /// `sourceMappingURL` comment is removed: nothing is emitted for a
    /// `.read()`, so it would point at a missing file.
    #[func(contextual)]
    fn read(
        engine: &mut ::typst::engine::Engine,
        context: ::comemo::Tracked<::typst::foundations::Context>,
        this: Content,
        /// The encoding to read the asset with. If `{none}`, returns raw
        /// bytes; otherwise the bytes are decoded as UTF-8 into a string.
        #[named]
        #[default(Some(Encoding::Utf8))]
        encoding: Option<Encoding>,
    ) -> HintedStrResult<Readable> {
        let elem = this.into_packed::<JsAsset>().unwrap();
        let spec = spec(&elem, context.styles()?)?;
        let sourcemap = matches!(&spec, AssetSpec::Js { options, .. } if options.sourcemap);
        let Some(asset) = introspect_asset(engine, spec, elem.span(), vec![OutputReq::Read]) else {
            return Ok(decode_empty(encoding));
        };
        if let Some(chunk) = asset.built.companions.iter().find(|c| c.chunk) {
            return Err(HintedString::new(eco_format!(
                "this script imports a chunk (`{}`), so it can't be inlined",
                chunk.path.rsplit('/').next().unwrap_or(&chunk.path)
            ))
            .with_hint("an inline script can't resolve relative chunk imports")
            .with_hint("link it with `.url()`, or bundle into one file with `split: false`"));
        }
        let bytes = asset
            .built
            .emit
            .read()
            .map_err(|err| eco_format!("failed to read asset bytes: {err}"))?;
        // `.read()` alone emits nothing, so the sourcemap the code points at
        // wouldn't exist: drop the directive rather than ship a dangling one.
        let bytes = match sourcemap {
            true => strip_sourcemap_directive(bytes),
            false => bytes,
        };
        decode(bytes, encoding)
    }

    /// The URLs of the chunks the entry imports statically, directly or
    /// through other chunks — for `<link rel="modulepreload">`, so the
    /// browser fetches them in parallel with the entry instead of
    /// discovering them one import at a time. Excludes the entry itself and
    /// chunks only reached through a dynamic `import()`. Marks the entry
    /// (and so its chunks) for emission, like `.url()`.
    #[func(contextual)]
    fn preload(
        engine: &mut ::typst::engine::Engine,
        context: ::comemo::Tracked<::typst::foundations::Context>,
        this: Content,
    ) -> HintedStrResult<Array> {
        let elem = this.into_packed::<JsAsset>().unwrap();
        let spec = spec(&elem, context.styles()?)?;
        Ok(
            match introspect_asset(engine, spec, elem.span(), vec![OutputReq::Auto]) {
                Some(asset) => asset
                    .preload
                    .iter()
                    .map(|url| Value::Str(url.as_str().into()))
                    .collect(),
                None => Array::new(),
            },
        )
    }
}

/// Remove the trailing `//# sourceMappingURL=..` line rolldown appends to a
/// chunk built with `sourcemap: true`.
fn strip_sourcemap_directive(bytes: Bytes) -> Bytes {
    const DIRECTIVE: &[u8] = b"\n//# sourceMappingURL=";
    match bytes
        .windows(DIRECTIVE.len())
        .rposition(|window| window == DIRECTIVE)
    {
        Some(at) => Bytes::new(bytes[..at].to_vec()),
        None => bytes,
    }
}

/// Default show: a bare `asset.js(..)` emits (with its chunks) and renders
/// nothing.
pub const SHOW_RULE: ShowFn<JsAsset> = |elem, engine, styles| {
    emit_or_request(engine, spec(elem, styles), elem.span(), Ok(OutputReq::Auto))
};

/// `format`: what kind of script to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Cast)]
pub enum Format {
    /// An ES module.
    Esm,
    /// An immediately-invoked function expression: a classic script.
    Iife,
}

/// `platform`: the environment the bundle runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Cast)]
pub enum Platform {
    Browser,
    Node,
    Neutral,
}

impl From<Platform> for rolldown::Platform {
    fn from(value: Platform) -> Self {
        match value {
            Platform::Browser => Self::Browser,
            Platform::Node => Self::Node,
            Platform::Neutral => Self::Neutral,
        }
    }
}

/// Every `asset.js` option except the entry, normalized at spec time to the
/// plain strings rolldown takes (typst values already JSON-encoded), so it's
/// hashable and comparable. Entries with equal options can share a build
/// ([`build_group`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JsOptions {
    pub minify: bool,
    pub sourcemap: bool,
    pub target: Option<EcoString>,
    pub format: Format,
    /// Resolved from `Smart`: always `false` for `iife`.
    pub split: bool,
    pub platform: Platform,
    pub external: Vec<EcoString>,
    /// Flattened `define`: dotted key → JSON-encoded leaf, in dict order.
    pub define: Vec<(EcoString, EcoString)>,
    /// Virtual module specifier → its JSON source.
    pub modules: Vec<(EcoString, EcoString)>,
    /// Global name → (module specifier, export name; `None` = default).
    pub inject: Vec<(EcoString, EcoString, Option<EcoString>)>,
}

/// An `inject` value: a specifier (its default export) or
/// `(specifier, export)`.
struct InjectFrom(EcoString, Option<EcoString>);

cast! {
    InjectFrom,
    v: EcoString => Self(v, None),
    v: Array => match v.as_slice() {
        [Value::Str(module), Value::Str(export)] => {
            Self(module.as_str().into(), Some(export.as_str().into()))
        }
        _ => typst::diag::bail!(
            "expected a module specifier or a `(specifier, export)` array of two strings"
        ),
    },
}

/// Build an `asset.js` element's [`AssetSpec`] from its (style-resolved)
/// fields.
fn spec(elem: &Packed<JsAsset>, styles: StyleChain) -> HintedStrResult<AssetSpec> {
    let entry = match &elem.path {
        AssetInput::Path(path) => {
            let entry = resolve_path(path, elem.span())?;
            // Bundling reads only the project (see `SandboxFs`), and the
            // resolver groups entries by their project path.
            if !matches!(entry.root(), VirtualRoot::Project) {
                return Err(HintedString::new(
                    "asset.js can only bundle a project file, not a package file".into(),
                )
                .with_hint(
                    "call asset.js with a path from the project, not from inside a package",
                ));
            }
            entry
        }
        AssetInput::Bytes(_) => {
            return Err(HintedString::new("asset.js needs a file path, not bytes".into())
                .with_hint("imports resolve relative to the entry's location, so it must be a project file"));
        }
    };

    let format = elem.format.get(styles);
    let split = match (format, elem.split.get(styles)) {
        (Format::Iife, Smart::Custom(true)) => {
            return Err(HintedString::new(
                "`split: true` can't be used with `format: \"iife\"`".into(),
            )
            .with_hint("a classic script can't load chunks; use `format: \"esm\"` to split"));
        }
        (Format::Iife, _) => false,
        (Format::Esm, split) => split.unwrap_or(true),
    };

    let mut define = Vec::new();
    flatten_define("", &elem.define.get_cloned(styles), &mut define)?;

    let mut modules = Vec::new();
    for (specifier, value) in elem.modules.get_cloned(styles) {
        modules.push((specifier.as_str().into(), json(&value)?));
    }

    let mut inject = Vec::new();
    for (name, value) in elem.inject.get_cloned(styles) {
        let InjectFrom(module, export) = value.cast::<InjectFrom>()?;
        inject.push((name.as_str().into(), module, export));
    }

    Ok(AssetSpec::Js {
        entry,
        options: JsOptions {
            minify: elem.minify.get(styles),
            sourcemap: elem.sourcemap.get(styles),
            target: elem.target.get_cloned(styles),
            format,
            split,
            platform: elem.platform.get(styles),
            external: elem.external.get_cloned(styles),
            define,
            modules,
            inject,
        },
    })
}

/// Flatten a `define` dict into dotted keys with JSON-encoded leaves.
/// rolldown only constant-folds a define whose value is a literal: with
/// `SITE` = `{"debug":false}`, `SITE.debug` survives minification, but with
/// `SITE.debug` = `false` an `if (SITE.debug)` branch is removed.
fn flatten_define(
    prefix: &str,
    dict: &Dict,
    out: &mut Vec<(EcoString, EcoString)>,
) -> HintedStrResult<()> {
    for (key, value) in dict {
        let key: EcoString = if prefix.is_empty() {
            key.as_str().into()
        } else {
            eco_format!("{prefix}.{key}")
        };
        match value {
            Value::Dict(inner) if !inner.is_empty() => flatten_define(&key, inner, out)?,
            _ => {
                if out.iter().any(|(k, _)| *k == key) {
                    return Err(eco_format!(
                        "`define` sets `{key}` twice (a nested dictionary and a dotted key \
                         name the same thing)"
                    )
                    .into());
                }
                out.push((key, json(value)?))
            }
        }
    }
    Ok(())
}

/// JSON-encode a typst value (as `json.encode` would, compactly).
fn json(value: &Value) -> HintedStrResult<EcoString> {
    serde_json::to_string(value)
        .map(EcoString::from)
        .map_err(|err| eco_format!("failed to encode {} as JSON: {err}", value.ty()).into())
}

// ---------------------------------------------------------------------------
// Building
// ---------------------------------------------------------------------------

/// Every `asset.js` entry of one group, built together: each entry's
/// [`Built`] (in input order), or the errors that failed the whole group.
/// Warnings are kept on failure too.
pub(crate) struct GroupBuilt {
    pub result: Result<Vec<Built>, Vec<String>>,
    /// Rendered rolldown warnings (`file:line` and a code frame).
    pub warnings: Vec<String>,
    /// rolldown builds run: one per `tsconfig.json` governing the entries
    /// (normally one).
    pub rolldown_builds: usize,
}

/// Build a group of `asset.js` entries (absolute paths inside `ctx.root`)
/// that share one set of `options`: one rolldown build, so modules they
/// share become shared chunks. Every file access is recorded in `rec`,
/// including when the build fails (or panics).
///
/// TypeScript/JSX transform options come from the `tsconfig.json`
/// governing each entry, found through the sandbox and merged here
/// (rolldown's own per-file discovery would read it with an unsandboxed
/// resolver). One tsconfig applies to every module in a rolldown build, so
/// entries governed by different tsconfig files are split into one rolldown
/// build per tsconfig: the one exception to "one build per option set", and
/// modules aren't shared between those builds. The group fails if any of
/// them does, but every one is attempted, so every entry's deps are
/// recorded and every error is reported.
///
/// Each entry's code is its asset; the chunks and sourcemaps it reaches are
/// its [companions](Built::companions); the chunks it statically imports
/// are its [`Built::preload`].
pub(crate) fn build_group(
    ctx: &TwylaContext,
    entries: &[PathBuf],
    options: &JsOptions,
    rec: &DepRecorder,
) -> GroupBuilt {
    let root = &ctx.root;
    // rolldown's filesystem must be `Clone + 'static`, so it records into a
    // recorder of its own, folded into `rec` when this guard drops: on every
    // return path, and on unwind if rolldown panics.
    let local = Arc::new(DepRecorder::new());
    let _absorb = Absorb {
        into: rec,
        from: Arc::clone(&local),
    };
    let fs = SandboxFs::new(root.to_path_buf(), local);
    let resolver = new_resolver(root, options, fs.clone());
    let mut warnings = Vec::new();
    let mut rolldown_builds = 0;
    let render = |diag: &BuildDiagnostic| render_diagnostic(root, diag);

    // Partition the entries by governing tsconfig, in entry order. A
    // lookup error doesn't stop the others: every error is reported, and
    // every partition that can be built is (so its deps are recorded).
    let mut errors = Vec::new();
    let mut partitions: Vec<(Option<Arc<oxc_resolver::TsConfig>>, Vec<usize>)> = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let found = match resolver.clone_default_resolver().find_tsconfig(entry) {
            Ok(found) => found,
            Err(err) => {
                errors.push(format!("failed to load tsconfig: {err}"));
                continue;
            }
        };
        let same = |(t, _): &&mut (Option<Arc<oxc_resolver::TsConfig>>, Vec<usize>)| {
            t.as_ref().map(|t| &t.path) == found.as_ref().map(|f| &f.path)
        };
        match partitions.iter_mut().find(same) {
            Some((_, members)) => members.push(i),
            None => partitions.push((found, vec![i])),
        }
    }

    let mut built: Vec<Option<Built>> = vec![None; entries.len()];
    for (tsconfig, members) in partitions {
        let mut transform = BundlerTransformOptions {
            target: options.target.as_ref().map(|t| Either::Left(t.to_string())),
            ..Default::default()
        };
        if let Some(found) = &tsconfig {
            let (merged, merge_warnings) = merge_tsconfig(transform, found, true);
            transform = merged;
            warnings.extend(merge_warnings.iter().map(render));
        }
        let paths: Vec<PathBuf> = members.iter().map(|&i| entries[i].clone()).collect();
        rolldown_builds += 1;
        // Always driven from a thread of our own: `block_on` panics on a
        // thread that's already inside a tokio runtime, which the compile's
        // caller may be (an async handler, `#[tokio::test]`). Scoped, so the
        // build can borrow; a rolldown panic resurfaces here, on join.
        let output = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("twyla-rolldown-build".into())
                .spawn_scoped(scope, || {
                    runtime().block_on(bundle(
                        root,
                        &paths,
                        options,
                        transform,
                        fs.clone(),
                        Arc::clone(&resolver),
                        &mut warnings,
                    ))
                })
                .expect("failed to spawn the rolldown build thread")
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
        match output {
            Ok(output) => {
                for (k, &i) in members.iter().enumerate() {
                    built[i] = Some(entry_built(&output, &output.entries[k], &entries[i]));
                }
            }
            Err(errs) => errors.extend(errs),
        }
    }
    let result = if errors.is_empty() {
        Ok(built
            .into_iter()
            .map(|b| b.expect("every entry built"))
            .collect())
    } else {
        Err(errors)
    };
    GroupBuilt {
        result,
        warnings,
        rolldown_builds,
    }
}

/// One entry's [`Built`], out of the build that produced its `entry_file`.
fn entry_built(output: &GroupOutput, entry_file: &str, entry: &Path) -> Built {
    let stem = entry
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_owned);
    let entry_bytes = &output.files[entry_file].bytes;
    let companions = output
        .reachable(entry_file)
        .into_iter()
        .map(|name| {
            let file = &output.files[&name];
            Companion {
                path: format!("assets/{name}"),
                sha256: sha256(&file.bytes),
                emit: Emit::Bytes(file.bytes.clone()),
                chunk: matches!(file.kind, FileKind::Chunk { .. }),
            }
        })
        .collect();
    let preload = output
        .static_imports(entry_file)
        .into_iter()
        .map(|name| format!("assets/{name}"))
        .collect();
    Built {
        sha256: sha256(entry_bytes),
        emit: Emit::Bytes(entry_bytes.clone()),
        ext: Some("js".to_string()),
        stem,
        dimensions: None,
        elem_id: None,
        output_path: Some(format!("assets/{entry_file}")),
        companions,
        preload,
    }
}

/// A rolldown diagnostic as text: `file:line` (relative to `root`) and a
/// code frame.
fn render_diagnostic(root: &Path, diag: &BuildDiagnostic) -> String {
    diag.to_diagnostic_with(&DiagnosticOptions {
        cwd: root.to_path_buf(),
    })
    .convert_to_string(false)
}

/// Every file one rolldown build produced, by rolldown's file name (flat:
/// `main-Ab12Cd34.js`, `main-Ab12Cd34.js.map`).
struct GroupOutput {
    /// The entry chunk of each input entry, in input order.
    entries: Vec<String>,
    files: BTreeMap<String, OutputFile>,
}

struct OutputFile {
    bytes: Bytes,
    kind: FileKind,
}

enum FileKind {
    Chunk {
        /// Chunks this one imports statically, by file name (externals
        /// excluded).
        imports: Vec<String>,
        /// Chunks this one loads with `import()`, by file name.
        dynamic_imports: Vec<String>,
        /// Its sourcemap's file name, when sourcemaps are on.
        map: Option<String>,
    },
    SourceMap,
}

impl GroupOutput {
    /// The chunks `file` imports statically, transitively, in breadth-first
    /// order; `file` itself excluded.
    pub fn static_imports(&self, file: &str) -> Vec<String> {
        self.walk(file, false)
    }

    /// Every file that must be emitted with `file` for it to run: the chunks
    /// it reaches through static *and* dynamic imports, transitively, plus
    /// each one's sourcemap (and `file`'s own). `file` itself excluded.
    pub fn reachable(&self, file: &str) -> Vec<String> {
        let chunks = self.walk(file, true);
        let mut out = Vec::new();
        for name in std::iter::once(file).chain(chunks.iter().map(String::as_str)) {
            if name != file {
                out.push(name.to_string());
            }
            if let Some(OutputFile {
                kind: FileKind::Chunk { map: Some(map), .. },
                ..
            }) = self.files.get(name)
            {
                out.push(map.clone());
            }
        }
        out
    }

    fn walk(&self, file: &str, dynamic: bool) -> Vec<String> {
        let mut seen = vec![file.to_string()];
        let mut queue = VecDeque::from([file.to_string()]);
        while let Some(name) = queue.pop_front() {
            let Some(OutputFile {
                kind:
                    FileKind::Chunk {
                        imports,
                        dynamic_imports,
                        ..
                    },
                ..
            }) = self.files.get(&name)
            else {
                continue;
            };
            let next = imports
                .iter()
                .chain(dynamic.then_some(dynamic_imports).into_iter().flatten());
            for import in next {
                if !seen.contains(import) {
                    seen.push(import.clone());
                    queue.push_back(import.clone());
                }
            }
        }
        seen.remove(0);
        seen
    }
}

/// The module resolver for a group's builds, reading through `fs`.
fn new_resolver(
    root: &Path,
    options: &JsOptions,
    fs: SandboxFs,
) -> Arc<rolldown_resolver::Resolver<SandboxFs>> {
    // The resolver's options must agree with the bundler's: rolldown
    // normalizes these for the resolver it builds itself (adding `module` to
    // the conditions for browser/node), but we build ours.
    let resolve_options = ResolveOptions {
        condition_names: matches!(options.platform, Platform::Browser | Platform::Node)
            .then(|| vec!["module".to_string()]),
        ..Default::default()
    };
    let tsconfig = TsConfig::Auto(true).with_base(root);
    Arc::new(rolldown_resolver::Resolver::new(
        fs,
        root.to_path_buf(),
        rolldown::Platform::from(options.platform),
        &tsconfig,
        resolve_options,
    ))
}

/// One rolldown build of `entries`, with the tsconfig-derived `transform`.
async fn bundle(
    root: &Path,
    entries: &[PathBuf],
    options: &JsOptions,
    transform: BundlerTransformOptions,
    fs: SandboxFs,
    resolver: Arc<rolldown_resolver::Resolver<SandboxFs>>,
    warnings: &mut Vec<String>,
) -> Result<GroupOutput, Vec<String>> {
    let render = |diag: &BuildDiagnostic| render_diagnostic(root, diag);
    let platform = rolldown::Platform::from(options.platform);
    let input = entries
        .iter()
        .map(|entry| InputItem {
            name: None,
            import: entry.to_string_lossy().into_owned(),
        })
        .collect();
    let bundler_options = BundlerOptions {
        input: Some(input),
        cwd: Some(root.to_path_buf()),
        platform: Some(platform),
        format: Some(match options.format {
            Format::Esm => OutputFormat::Esm,
            Format::Iife => OutputFormat::Iife,
        }),
        code_splitting: Some(CodeSplittingMode::Bool(options.split)),
        minify: Some(RawMinifyOptions::Bool(options.minify)),
        sourcemap: options.sourcemap.then_some(SourceMapType::File),
        external: (!options.external.is_empty()).then(|| {
            IsExternal::from(
                options
                    .external
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            )
        }),
        define: Some(
            options
                .define
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ),
        inject: Some(
            options
                .inject
                .iter()
                .map(|(name, module, export)| match export {
                    None => InjectImport::default(name.to_string(), module.to_string()),
                    Some(export) => InjectImport::named(
                        export.to_string(),
                        Some(name.to_string()),
                        module.to_string(),
                    ),
                })
                .collect(),
        ),
        // `false`: the transform takes the tsconfig merged above, instead of
        // discovering one per file behind the sandbox's back.
        tsconfig: Some(TsConfig::Auto(false)),
        transform: Some(transform),
        // Flat, next to each other: the entry imports chunks as `./name`.
        //
        // One file name must always mean the same bytes (two builds sharing
        // a chunk write it once; render rejects one path with two digests).
        // rolldown's `[hash]` covers the code as minified and after every
        // option that changes it (format, define, inject, target, …) —
        // except the `//# sourceMappingURL` comment, which it appends after
        // hashing. So the same module built with and without sourcemaps
        // would get one name and two contents: the sourcemap variant gets
        // its own `.sm.js` names instead.
        entry_filenames: Some(filename_template(options.sourcemap).into()),
        chunk_filenames: Some(filename_template(options.sourcemap).into()),
        ..Default::default()
    };

    let refused = Arc::new(Mutex::new(Vec::new()));
    let plugin = TwylaPlugin {
        modules: options
            .modules
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        refused: Arc::clone(&refused),
    };
    let mut factory = BundleFactory::new(BundleFactoryOptions {
        bundler_options,
        plugins: vec![Plugin::new_shared(plugin)],
        disable_tracing_setup: true,
        ..Default::default()
    })
    .map_err(|errs| errs.iter().map(render).collect::<Vec<_>>())?;
    let bundle = factory.create_bundle_with_fs(fs, resolver);
    let out = bundle.generate().await.map_err(|errs| {
        // rolldown reports a plugin error as just "plugin `twyla` threw an
        // error" at the import: put the plugin's reason first.
        let mut messages = std::mem::take(&mut *refused.lock().unwrap());
        messages.extend(errs.iter().map(render));
        messages
    })?;
    // rolldown leaves an import it can't resolve in the output as an external
    // (with a warning), which would fail at runtime. Externals are explicit
    // here (`external`), so it's an error.
    let (unresolved, rest): (Vec<_>, Vec<_>) = out
        .warnings
        .iter()
        .partition(|w| matches!(w.kind(), EventKind::UnresolvedImport));
    warnings.extend(rest.into_iter().map(render));
    if !unresolved.is_empty() {
        return Err(unresolved
            .into_iter()
            .map(|w| {
                format!(
                    "{}\nto leave it as a runtime import, list it in `external`",
                    render(w)
                )
            })
            .collect());
    }

    let mut files = BTreeMap::new();
    let mut entry_chunks: Vec<(String, String)> = Vec::new();
    for output in &out.assets {
        match output {
            Output::Chunk(chunk) => {
                let name = chunk.filename.to_string();
                let known = |names: &[ArcStr]| -> Vec<String> {
                    names.iter().map(ArcStr::to_string).collect()
                };
                if chunk.is_entry
                    && let Some(facade) = &chunk.facade_module_id
                {
                    entry_chunks.push((facade.to_string(), name.clone()));
                }
                files.insert(
                    name,
                    OutputFile {
                        bytes: Bytes::new(chunk.code.clone().into_bytes()),
                        kind: FileKind::Chunk {
                            imports: known(&chunk.imports),
                            dynamic_imports: known(&chunk.dynamic_imports),
                            map: chunk.sourcemap_filename.clone(),
                        },
                    },
                );
            }
            Output::Asset(asset) if asset.filename.ends_with(".map") => {
                files.insert(
                    asset.filename.to_string(),
                    OutputFile {
                        bytes: Bytes::new(asset.source.as_bytes().to_vec()),
                        kind: FileKind::SourceMap,
                    },
                );
            }
            Output::Asset(asset) => {
                return Err(vec![format!(
                    "rolldown emitted an unsupported file `{}`; asset.js only bundles JavaScript",
                    asset.filename
                )]);
            }
        }
    }
    // A chunk's `imports`/`dynamic_imports` list the chunks it imports by file
    // name *and* its externals by specifier, in one list, with nothing
    // marking which is which. Keep the entries that exactly name a chunk of
    // this build. That's only unambiguous if no external is spelled like one
    // of those chunk names: rolldown's implicit externals can't be (http(s)
    // URLs have a `:` or start with `//`, Node builtins have no `.js`), and
    // an `external` entry that is, is refused.
    let names: Vec<String> = files
        .iter()
        .filter(|(_, file)| matches!(file.kind, FileKind::Chunk { .. }))
        .map(|(name, _)| name.clone())
        .collect();
    if let Some(clash) = options
        .external
        .iter()
        .find(|e| names.contains(&e.to_string()))
    {
        return Err(vec![format!(
            "the external `{clash}` has the same name as a chunk of this bundle, so imports of \
             the two can't be told apart; rename it"
        )]);
    }
    for file in files.values_mut() {
        if let FileKind::Chunk {
            imports,
            dynamic_imports,
            ..
        } = &mut file.kind
        {
            imports.retain(|i| names.contains(i));
            dynamic_imports.retain(|i| names.contains(i));
        }
    }

    let mut entry_files = Vec::new();
    for entry in entries {
        // rolldown reports the facade by its resolved (canonical) path.
        let canonical = std::fs::canonicalize(entry).unwrap_or_else(|_| entry.clone());
        let found = entry_chunks.iter().find(|(facade, _)| {
            Path::new(facade) == entry.as_path() || Path::new(facade) == canonical.as_path()
        });
        match found {
            Some((_, name)) => entry_files.push(name.clone()),
            None => {
                return Err(vec![format!(
                    "rolldown produced no chunk for entry {}",
                    entry.display()
                )]);
            }
        }
    }
    Ok(GroupOutput {
        entries: entry_files,
        files,
    })
}

/// rolldown's file name template for entries and chunks. See where it's used.
fn filename_template(sourcemap: bool) -> String {
    if sourcemap {
        "[name]-[hash].sm.js".to_string()
    } else {
        "[name]-[hash].js".to_string()
    }
}

/// The tokio runtime rolldown's async build runs on, created on first use and
/// shared by every build. Multi-threaded: rolldown spawns module-loading tasks
/// (`tokio::spawn`) and blocking file reads (`spawn_blocking`), and some of
/// its helpers use `block_in_place`, which a current-thread runtime forbids.
/// Each build is driven with `block_on` from a thread of its own (see
/// [`build_group`]), never from inside another runtime.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .thread_name("twyla-rolldown")
            .build()
            .expect("failed to start the tokio runtime for rolldown")
    })
}

/// Folds a build-local recorder into the build's recorder on drop. See
/// [`build_group`].
struct Absorb<'a> {
    into: &'a DepRecorder,
    from: Arc<DepRecorder>,
}

impl Drop for Absorb<'_> {
    fn drop(&mut self) {
        self.into.absorb(&self.from);
    }
}

// ---------------------------------------------------------------------------
// The sandbox filesystem
// ---------------------------------------------------------------------------

/// The only view of the disk rolldown gets (both its own file loads and its
/// resolver's). See the module docs.
///
/// - Paths outside `root` — lexically, or because a symlink inside `root`
///   points out of it — are `NotFound`: the resolver walks up through
///   ancestor directories looking for `node_modules`, `package.json` and
///   `tsconfig.json`, and treats `NotFound` as "keep going". Lexically
///   outside paths aren't recorded (no change there can affect the build);
///   an escaping symlink is, so replacing it is noticed.
/// - Every symlink the resolver goes through is recorded by identity (see
///   [`record_links`](Self::record_links)), so retargeting one is noticed.
/// - Reads record a ledger read, `metadata`/`exists`/link lookups a probe,
///   and `read_dir` a listing — through [`DepRecorder`], at access time.
/// - Every write fails with `PermissionDenied` (`generate` never writes, but
///   nothing relies on that).
///
/// The threat model is untrusted *source code* — the scripts and their
/// imports, like the typst sources — not an attacker racing filesystem
/// writes inside the project. So confinement is a check (lexical, then
/// canonical) followed by an ordinary open by path, as for typst's own file
/// loading and twyla's other assets; a symlink swapped in between the two
/// isn't defended against (that would take `openat`-style confinement).
///
/// pnpm-style `node_modules` (symlinks into a store outside the project)
/// are refused by the symlink check; npm/yarn/bun's flat layout works.
#[derive(Clone)]
struct SandboxFs {
    root: PathBuf,
    rec: Arc<DepRecorder>,
    /// For the operations [`DepRecorder`] has no equivalent of
    /// (`symlink_metadata`, `read_link`, `canonicalize`), done after the
    /// access is recorded.
    os: OsFileSystem,
}

impl SandboxFs {
    fn new(root: PathBuf, rec: Arc<DepRecorder>) -> Self {
        Self {
            root,
            rec,
            os: <OsFileSystem as OxcFileSystem>::new(false),
        }
    }

    /// Admit `path` (returning it lexically normalized) or refuse it with
    /// `NotFound`. See the type docs.
    fn admit(&self, path: &Path) -> io::Result<PathBuf> {
        let path = normalize(path);
        if !path.starts_with(&self.root) {
            return Err(outside());
        }
        match std::fs::canonicalize(&path) {
            Ok(real) if !real.starts_with(&self.root) => {
                self.record_links(&path);
                Err(outside())
            }
            // Missing paths are admitted: the access itself reports (and
            // records) that they're missing.
            _ => Ok(path),
        }
    }

    /// Record every symlink on `path` inside the root (itself and its
    /// ancestors) as a [`DepKind::Stat`](crate::ledger::DepKind::Stat) dep.
    /// The resolver follows links to their canonical target and rolldown
    /// reads *that*, so the ledger's reads name the old target: without
    /// these, retargeting a link (to another in-project file, or from an
    /// escaping target to a valid one) would leave the stale bundle — or the
    /// stale failure — cached.
    fn record_links(&self, path: &Path) {
        for ancestor in path.ancestors() {
            if ancestor == self.root || !ancestor.starts_with(&self.root) {
                break;
            }
            if std::fs::symlink_metadata(ancestor).is_ok_and(|m| m.is_symlink()) {
                self.rec.stat(ancestor);
            }
        }
    }

    fn probe(&self, path: &Path) -> io::Result<PathBuf> {
        let path = self.admit(path)?;
        match self.rec.metadata(&path) {
            Some(_) => Ok(path),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "not found")),
        }
    }
}

fn outside() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "outside the twyla project root")
}

fn refuse_write() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "asset.js builds may not write files",
    )
}

/// Resolve `.` and `..` components without touching the disk, so
/// `<root>/../etc` isn't mistaken for a path inside `<root>`.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

impl OxcFileSystem for SandboxFs {
    fn new(_yarn_pnp: bool) -> Self {
        // Only called for a resolver built without a filesystem, or with
        // yarn PnP toggled; twyla always passes its own and never enables PnP.
        unreachable!("twyla's SandboxFs is always constructed explicitly")
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let path = self.admit(path)?;
        self.rec.read(&path)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        let path = self.admit(path)?;
        self.rec
            .metadata(&path)
            .map(FileMetadata::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        let path = self.admit(path)?;
        self.rec.metadata(&path);
        self.record_links(&path);
        self.os.symlink_metadata(&path)
    }

    fn read_link(&self, path: &Path) -> Result<PathBuf, ResolveError> {
        let path = self.admit(path)?;
        self.rec.metadata(&path);
        self.record_links(&path);
        self.os.read_link(&path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let path = self.probe(path)?;
        self.record_links(&path);
        self.os.canonicalize(&path)
    }
}

impl rolldown_fs::FileSystem for SandboxFs {
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(refuse_write())
    }

    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        Err(refuse_write())
    }

    fn write(&self, _: &Path, _: &[u8]) -> io::Result<()> {
        Err(refuse_write())
    }

    fn remove_file(&self, _: &Path) -> io::Result<()> {
        Err(refuse_write())
    }

    fn exists(&self, path: &Path) -> bool {
        self.probe(path).is_ok()
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let path = self.admit(path)?;
        let names = self.rec.list_dir(&path)?;
        Ok(names.into_iter().map(|name| path.join(name)).collect())
    }
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

/// twyla's one rolldown plugin. It serves the `modules` virtual modules, and
/// it's the gate for real files: a module that isn't JavaScript, TypeScript,
/// JSON or text — a stylesheet, an image, anything imported as an asset
/// (`new URL("./x.png", import.meta.url)`) — is an error. rolldown would
/// otherwise emit it as an asset file, read with `tokio::fs` behind the
/// sandbox's back.
#[derive(Debug)]
struct TwylaPlugin {
    /// Specifier → JSON source.
    modules: BTreeMap<String, String>,
    /// Why each refused module was refused. rolldown's diagnostic for a
    /// plugin error drops the error's message, so the build reports these.
    refused: Arc<Mutex<Vec<String>>>,
}

/// Extensions a real module may have.
const MODULE_EXTENSIONS: &[&str] = &[
    "js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx", "json", "txt",
];

impl Plugin for TwylaPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("twyla")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load
    }

    async fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        Ok(self
            .modules
            .contains_key(args.specifier)
            .then(|| HookResolveIdOutput {
                // The `\0` prefix is the rollup convention for "not a file".
                id: format!("\0{}", args.specifier).into(),
                ..Default::default()
            }))
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        if let Some(specifier) = args.id.strip_prefix('\0') {
            return Ok(self.modules.get(specifier).map(|json| HookLoadOutput {
                code: ArcStr::from(json.as_str()),
                module_type: Some(ModuleType::Json),
                ..Default::default()
            }));
        }
        // Only real files are gated (not `data:` URLs).
        let file = args.id.split(['?', '#']).next().unwrap_or(args.id);
        if !Path::new(file).is_absolute() {
            return Ok(None);
        }
        let ext = Path::new(file)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let as_asset = matches!(
            args.asserted_module_type,
            Some(ModuleType::Asset | ModuleType::Copy | ModuleType::Css)
        );
        if as_asset || !MODULE_EXTENSIONS.contains(&ext) {
            let message = format!(
                "can't import `{file}`: asset.js only bundles JavaScript, TypeScript, JSON and \
                 text modules (load stylesheets and images with the other `asset` types)"
            );
            self.refused.lock().unwrap().push(message.clone());
            anyhow::bail!(message);
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests;

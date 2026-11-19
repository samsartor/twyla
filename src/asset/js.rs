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
use rolldown_common::{
    EmittedChunk, ExperimentalOptions, ImportKind, Output, ResolvedExternal, ResolvedId,
};
use rolldown_error::{BuildDiagnostic, DiagnosticOptions, EventKind};
use rolldown_fs::OsFileSystem;
use rolldown_plugin::{
    HookGenerateBundleArgs, HookLoadArgs, HookLoadOutput, HookLoadReturn, HookNoopReturn,
    HookResolveFileUrlArgs, HookResolveFileUrlReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookTransformAstArgs, HookTransformAstReturn, HookUsage, Plugin,
    PluginContext, SharedLoadPluginContext,
};
use typst::diag::{HintedStrResult, HintedString};
use typst::foundations::{
    Array, Bytes, Cast, Content, Dict, Packed, ShowFn, Smart, Str, StyleChain, Value, cast, elem,
    func, scope,
};
use typst::loading::{Encoding, Readable};
use typst::syntax::VirtualRoot;

use super::{
    AssetInput, AssetSpec, Built, Companion, CompanionKind, OutputReq, decode, decode_empty,
    emit_or_request, introspect_asset, resolve_or_request, resolve_path, sha256,
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
/// conditions), and `tsconfig.json` `paths` aliases. A path starting with
/// `/` (`import { x } from "/scripts/x.ts"`) is relative to the project
/// root, as in typst, in every kind of import (static, dynamic, queried,
/// `inject`). TypeScript and JSX are
/// compiled using the `tsconfig.json` that governs the entry (its `jsx`,
/// `jsxImportSource`, `jsxFactory`, decorator options, …), applied to every
/// module in the bundle. Install packages with your package manager as
/// usual; twyla only reads `node_modules`, it never runs anything.
///
/// Every file the bundle reads must be inside the project: imports that
/// reach outside it (including through a symlink) fail to resolve. Only
/// JavaScript, TypeScript, JSON and text modules can be imported as code.
/// Any other file can be imported with a query, as in Vite:
///
/// - `import shader from "./shader.glsl?raw"`: the file's contents, as a
///   string (any file, including a `.ts` file, whose source isn't compiled;
///   it must be valid UTF-8).
/// - `import logo from "./logo.png?url"`: the file's URL. The file is
///   emitted exactly as `asset.file` would emit it by default, at the same
///   URL (always that default fingerprinted name: an `output:` given to an
///   `asset.file` of the same file doesn't apply), along with every entry
///   whose code imports it (also when the entry is only inlined with
///   `.read()`: the URL is absolute, so it works from anywhere). A file
///   imported by a relative path through a symlink is named after the link,
///   like `asset.file` names it; one reached through a package or a
///   `paths` alias is named after the file the link points to.
/// - `import icon from "./icon.svg?inline"`: the file as a `data:` URL
///   (`data:image/svg+xml;base64,…`), with its type from its extension.
///
/// An import takes exactly one of these queries, and no `#` fragment
/// (a leading `#`, a package import like `#internal/x.txt?raw`, is fine);
/// anything else is an error. For TypeScript, declare them once, e.g. in a `queries.d.ts`:
///
/// ```ts
/// declare module "*?raw" { const source: string; export default source; }
/// declare module "*?url" { const url: string; export default url; }
/// declare module "*?inline" { const url: string; export default url; }
/// declare module "*?worker" {
///   const W: new (options?: WorkerOptions) => Worker; export default W;
/// }
/// declare module "*?worker&url" { const url: string; export default url; }
/// ```
///
/// A web worker is a script of its own, bundled as an extra entry of the
/// same build (sharing chunks with the rest of it):
///
/// - `import W from "./worker.ts?worker"`: a function that starts it,
///   `new W()` (or `W()`) being `new Worker(url, {type: "module"})` (other
///   `Worker` options can be passed: `new W({name: "w"})`).
/// - `import url from "./worker.ts?worker&url"`: just its URL (the only
///   combination of queries allowed).
/// - `new Worker(new URL("./worker.ts", import.meta.url), { type: "module" })`
///   (or `new SharedWorker(..)`), with a string literal path and the
///   `type: "module"` option written out (the worker is bundled as a
///   module; without it, it's an error): the URL is rewritten to the
///   worker's file, relative to the script.
///
/// `?worker` URLs are twyla URLs, root-absolute with the `base_url`, like
/// `?url`'s: they work from wherever the importing code runs, an inlined
/// (`.read()`) script included. A `new URL` worker's URL is relative to
/// the script, so a script that starts one can't be inlined. The worker's
/// files are emitted along with every entry whose code (after
/// tree-shaking) starts it, directly or from a chunk it imports (like a
/// dynamic import: never in `.preload()`), and with an inlined entry too.
/// A worker needs a file of its own, so it takes code splitting: with
/// `split: false` or `format: "iife"`, it's an error.
///
/// Any other `new URL("<path>", import.meta.url)` with a string literal
/// path is an error (a script included: a download link isn't a worker):
/// the URL would resolve next to the bundled script in `assets/`, where
/// the file isn't. Import its URL with `?url` instead. A `new URL` of
/// anything but a string literal, or of a URL with a scheme (or
/// `//host/…`), is left as written, and so is `import.meta.url`.
///
/// Stylesheets are better loaded with `asset.sass` (or `asset.file`) and
/// linked from the page.
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
///
/// `import.meta.env.BASE_URL` is replaced with the prefix of every twyla
/// URL, with a trailing slash: the `base_url` (`"https://example.com/blog/"`),
/// or `"/"` without one. `define` overrides it. Twyla sets no other
/// `import.meta.env` value: there's no `MODE`, `DEV` or `PROD`, since twyla
/// has no development mode (`twyla serve` serves exactly what
/// `twyla build` writes). Code written for Vite that reads them can be
/// given them with `define`, and dead branches on them are removed:
///
/// ```typ
/// #asset.js("/main.ts", define: (
///   "import.meta.env": (MODE: "production", DEV: false, PROD: true),
/// ))
/// ```
///
/// In an `"iife"` script, which has no `import.meta`, any other
/// `import.meta` (`import.meta.url`, an `import.meta.env` value `define`
/// doesn't set) is an error.
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
    /// uses the global: a package name, a `virtual:` module, a path
    /// relative to that module, or a `/…` path, relative to the project
    /// root.
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
        // Its `?url` files and `?worker`s are referenced by absolute URLs,
        // which work from anywhere; a chunk referenced relatively doesn't.
        for companion in &asset.built.companions {
            let name = companion.path.rsplit('/').next().unwrap_or(&companion.path);
            match companion.kind {
                CompanionKind::Chunk => {
                    return Err(HintedString::new(eco_format!(
                        "this script imports a chunk (`{name}`), so it can't be inlined"
                    ))
                    .with_hint("an inline script can't resolve relative chunk imports")
                    .with_hint(
                        "link it with `.url()`, or bundle into one file with `split: false`",
                    ));
                }
                CompanionKind::RelativeWorker => {
                    return Err(HintedString::new(eco_format!(
                        "this script starts the worker `{name}` by a URL relative to the script \
                         (`new URL(.., import.meta.url)`), so it can't be inlined"
                    ))
                    .with_hint(
                        "an inline script's `import.meta.url` is the page's URL, not the script's",
                    )
                    .with_hint(
                        "link it with `.url()`, or import the worker with \
                         `import W from \"./worker.ts?worker\"`, whose URL is absolute",
                    ));
                }
                CompanionKind::SourceMap | CompanionKind::File | CompanionKind::Worker => {}
            }
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
                        ctx,
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
    let mut companions: Vec<Companion> = Vec::new();
    let mut add = |name: &str, kind: CompanionKind| {
        let path = format!("assets/{name}");
        if companions.iter().any(|c| c.path == path) {
            return;
        }
        let file = &output.files[name];
        companions.push(Companion {
            path,
            sha256: sha256(&file.bytes),
            emit: Emit::Bytes(file.bytes.clone()),
            kind,
        });
    };
    // The chunks the entry imports, statically or dynamically, by relative
    // URLs, and their sourcemaps (and the entry's).
    let imported = output.walk(entry_file, Follow::Imports);
    for name in std::iter::once(entry_file).chain(imported.iter().map(String::as_str)) {
        if name != entry_file {
            add(name, CompanionKind::Chunk);
        }
        if let Some(map) = output.map_of(name) {
            add(map, CompanionKind::SourceMap);
        }
    }
    // The workers those start, and everything each worker needs (its own
    // chunks and workers, and their sourcemaps). A worker the entry's code
    // starts relatively (`new URL(..)`) can't be inlined; one started by its
    // twyla URL (`?worker`) can.
    let starts = std::iter::once(entry_file).chain(imported.iter().map(String::as_str));
    let mut roots: Vec<&WorkerFile> = starts.flat_map(|name| output.workers_of(name)).collect();
    roots.sort();
    roots.dedup();
    for root in &roots {
        if root.relative {
            add(&root.file, CompanionKind::RelativeWorker);
        }
    }
    for root in &roots {
        let tree = std::iter::once(root.file.clone()).chain(output.walk(&root.file, Follow::All));
        for name in tree {
            if name == entry_file {
                continue;
            }
            add(&name, CompanionKind::Worker);
            if let Some(map) = output.map_of(&name) {
                add(map, CompanionKind::Worker);
            }
        }
    }
    // The `?url` files of every module in the entry's chunk or a chunk it
    // reaches (by an import or a worker).
    let chunks =
        std::iter::once(entry_file.to_string()).chain(output.walk(entry_file, Follow::All));
    for chunk in chunks {
        let Some(OutputFile {
            kind: FileKind::Chunk { urls, .. },
            ..
        }) = output.files.get(&chunk)
        else {
            continue;
        };
        for url in urls {
            if companions.iter().any(|c| c.path == url.output_path) {
                continue;
            }
            companions.push(Companion {
                path: url.output_path.clone(),
                // Like `asset.file`: a copy of the source, fingerprinted by
                // the hash streamed when the module was loaded.
                emit: Emit::Copy(url.source.clone()),
                sha256: url.sha256,
                kind: CompanionKind::File,
            });
        }
    }
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
        /// The `?url` files its modules reference, in module order.
        urls: Vec<UrlFile>,
        /// The worker entry chunks its code starts, in file-name order.
        workers: Vec<WorkerFile>,
    },
    SourceMap,
}

/// A file imported with `?url`: emitted as a companion of every entry that
/// reaches the chunk containing the importing module.
#[derive(Clone, Debug)]
struct UrlFile {
    /// Its output path, exactly as `asset.file` would name it.
    output_path: String,
    /// The file, as the build resolved (and read) it.
    source: PathBuf,
    sha256: [u8; 32],
}

/// Which references [`GroupOutput::walk`] follows from a chunk.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Follow {
    /// Static imports: what loads with the chunk.
    Static,
    /// Static and dynamic imports: what the chunk imports by relative URLs.
    Imports,
    /// Imports and the workers chunks start: everything that must be
    /// emitted for the chunk to run.
    All,
}

impl GroupOutput {
    /// The chunks `file` imports statically, transitively, in breadth-first
    /// order; `file` itself excluded.
    pub fn static_imports(&self, file: &str) -> Vec<String> {
        self.walk(file, Follow::Static)
    }

    fn map_of(&self, file: &str) -> Option<&str> {
        match self.files.get(file) {
            Some(OutputFile {
                kind: FileKind::Chunk { map: Some(map), .. },
                ..
            }) => Some(map),
            _ => None,
        }
    }

    fn workers_of(&self, file: &str) -> &[WorkerFile] {
        match self.files.get(file) {
            Some(OutputFile {
                kind: FileKind::Chunk { workers, .. },
                ..
            }) => workers,
            _ => &[],
        }
    }

    /// The chunks reached from `file` by `follow`, transitively, in
    /// breadth-first order; `file` itself excluded.
    fn walk(&self, file: &str, follow: Follow) -> Vec<String> {
        let mut seen = vec![file.to_string()];
        let mut queue = VecDeque::from([file.to_string()]);
        while let Some(name) = queue.pop_front() {
            let Some(OutputFile {
                kind:
                    FileKind::Chunk {
                        imports,
                        dynamic_imports,
                        workers,
                        ..
                    },
                ..
            }) = self.files.get(&name)
            else {
                continue;
            };
            let dynamic = match follow {
                Follow::Static => &[][..],
                Follow::Imports | Follow::All => &dynamic_imports[..],
            };
            let workers = workers
                .iter()
                .filter(|_| follow == Follow::All)
                .map(|w| &w.file);
            for next in imports.iter().chain(dynamic).chain(workers) {
                if !seen.contains(next) {
                    seen.push(next.clone());
                    queue.push_back(next.clone());
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
    ctx: &TwylaContext,
    entries: &[PathBuf],
    options: &JsOptions,
    transform: BundlerTransformOptions,
    fs: SandboxFs,
    resolver: Arc<rolldown_resolver::Resolver<SandboxFs>>,
    warnings: &mut Vec<String>,
) -> Result<GroupOutput, Vec<String>> {
    let root = &ctx.root;
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
        define: Some(define(ctx, options).into_iter().collect()),
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
        // Reports `new URL("<literal>", import.meta.url)` to the plugin as
        // an import, which resolves every one (a worker entry, an external,
        // or an error) before rolldown's asset handling could read the file.
        experimental: Some(ExperimentalOptions {
            resolve_new_url_to_asset: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };

    let refused = Arc::new(Mutex::new(Vec::new()));
    let urls = Arc::new(Mutex::new(BTreeMap::new()));
    let worker_files = Arc::new(Mutex::new(Vec::new()));
    let plugin = TwylaPlugin {
        modules: options
            .modules
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        refused: Arc::clone(&refused),
        ctx: ctx.clone(),
        fs: fs.clone(),
        urls: Arc::clone(&urls),
        resolver: Arc::clone(&resolver),
        split: options.split,
        new_urls: Mutex::new(BTreeMap::new()),
        emitted: Mutex::new(Vec::new()),
        worker_files: Arc::clone(&worker_files),
    };
    // `BundleFactory::new` also builds a resolver and a filesystem of its
    // own, on the OS. `create_bundle_with_fs` keeps them out of the module
    // loader, but plugin contexts still hold that resolver: `ctx.resolve`
    // and `ctx.try_get_package_json_or_create` would reach the disk outside
    // the sandbox. twyla's plugin never calls them (queried imports go
    // through `resolver` directly), and of rolldown's inner plugins only
    // `CopyModulePlugin` does, and it's inert without `moduleTypes`.
    // `AssetModulePlugin` reads with `tokio::fs`, but only modules typed
    // `asset`, which twyla's `load` refuses first (it runs `Post`).
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
    // Two warnings are errors, since the output would silently misbehave:
    // rolldown leaves an import it can't resolve in the output as an
    // external, which would fail at runtime (externals are explicit here,
    // `external`); and in an `iife` script it replaces an `import.meta` it
    // can't rewrite (`import.meta.url`, an `import.meta.env.X` that `define`
    // doesn't set) with `{}`.
    let mut errors = Vec::new();
    for warning in &out.warnings {
        match warning.kind() {
            EventKind::UnresolvedImport => errors.push(format!(
                "{}\nto leave it as a runtime import, list it in `external`",
                render(warning)
            )),
            EventKind::EmptyImportMeta => errors.push(format!(
                "{}\na classic (`iife`) script has no `import.meta`: use `format: \"esm\"`, or \
                 set the value with `define` (`import.meta.url` has no replacement: twyla URLs \
                 are root-absolute, `import.meta.env.BASE_URL` is the prefix)",
                render(warning)
            )),
            _ => warnings.push(render(warning)),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
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
                            // By the exact module id the plugin loaded it as.
                            urls: chunk
                                .module_ids
                                .iter()
                                .filter_map(|id| urls.lock().unwrap().get(id.as_str()).cloned())
                                .collect(),
                            // Found below, in the code.
                            workers: Vec::new(),
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
    // The workers each chunk starts: those whose reference survives in its
    // final code (so a worker whose start was tree-shaken away isn't
    // shipped). A worker's file name carries its content hash, so it
    // appears in another chunk's code only where the code references it:
    // as its twyla URL (`?worker`), or, relative to the chunk, as a string
    // of its own (`new URL("w-<hash>.js", import.meta.url)`). In file-name
    // order, so the traversal (and so the companions) don't depend on the
    // order the build emitted them in.
    let worker_files = std::mem::take(&mut *worker_files.lock().unwrap());
    for (name, file) in files.iter_mut() {
        let FileKind::Chunk {
            imports,
            dynamic_imports,
            workers,
            ..
        } = &mut file.kind
        else {
            continue;
        };
        imports.retain(|i| names.contains(i));
        dynamic_imports.retain(|i| names.contains(i));
        let code = String::from_utf8_lossy(&file.bytes);
        for worker in &worker_files {
            if worker.file == *name {
                continue;
            }
            let referenced = match worker.relative {
                true => ['"', '\'', '`'].iter().any(|q| {
                    code.contains(&format!("{q}{}{q}", worker.file))
                        || code.contains(&format!("{q}./{}{q}", worker.file))
                }),
                false => code.contains(&ctx.asset_url(&format!("assets/{}", worker.file))),
            };
            if referenced {
                workers.push(worker.clone());
            }
        }
    }

    let mut entry_files = Vec::new();
    for entry in entries {
        // rolldown reports the facade by its resolved (canonical) path.
        let canonical = std::fs::canonicalize(entry).unwrap_or_else(|_| entry.clone());
        // A module that's both an entry and a worker can be two entry
        // chunks (the configured one and the emitted one): the entry's is
        // the one that isn't a worker's.
        let is_worker = |name: &str| worker_files.iter().any(|w| w.file == name);
        let mut found: Vec<&(String, String)> = entry_chunks
            .iter()
            .filter(|(facade, _)| {
                Path::new(facade) == entry.as_path() || Path::new(facade) == canonical.as_path()
            })
            .collect();
        found.sort_by_key(|(_, name)| is_worker(name));
        let found = found.first();
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

/// The build's defines: the user's, plus `import.meta.env.BASE_URL` (the
/// prefix of every twyla URL, with a trailing slash) unless the user's
/// define sets it or something containing it.
fn define(ctx: &TwylaContext, options: &JsOptions) -> Vec<(String, String)> {
    const BASE_URL: &str = "import.meta.env.BASE_URL";
    let mut define: Vec<(String, String)> = options
        .define
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let covered = define.iter().any(|(key, _)| {
        BASE_URL == key
            || BASE_URL
                .strip_prefix(key.as_str())
                .is_some_and(|r| r.starts_with('.'))
    });
    if !covered {
        let base = serde_json::to_string(&ctx.asset_url("")).expect("a string serializes");
        define.push((BASE_URL.to_string(), base));
    }
    define
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
#[derive(Clone, Debug)]
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
    /// For `?url` imports' output paths and URLs.
    ctx: TwylaContext,
    /// For `?raw`/`?url`/`?inline` imports' reads, which the plugin does
    /// itself (confined and recorded like every other read).
    fs: SandboxFs,
    /// Every `?url` import loaded, by its module id.
    urls: Arc<Mutex<BTreeMap<String, UrlFile>>>,
    /// The bundle's resolver (over `fs`), for queried and root-absolute
    /// imports.
    resolver: Arc<rolldown_resolver::Resolver<SandboxFs>>,
    /// Whether the build splits code: a worker needs a file of its own.
    split: bool,
    /// Every `new URL("<literal>", import.meta.url)` in each module, by
    /// module id, found when the module is parsed (`transform_ast`), so its
    /// import can be told apart by the call it's in.
    new_urls: Mutex<BTreeMap<String, Vec<NewUrl>>>,
    /// Every worker entry emitted, by its reference id, and whether code
    /// references it relatively (`new URL(..)`) rather than by its twyla
    /// URL (`?worker`).
    emitted: Mutex<Vec<(ArcStr, bool)>>,
    /// The emitted workers' files, once named (`generate_bundle`).
    worker_files: Arc<Mutex<Vec<WorkerFile>>>,
}

/// One `new URL("<literal>", import.meta.url)` in a module.
#[derive(Clone, Debug)]
struct NewUrl {
    specifier: String,
    /// 1-based, in the module's source.
    line: usize,
    use_: NewUrlUse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NewUrlUse {
    /// `new Worker(new URL(..), { type: "module" })` (or `SharedWorker`).
    Worker,
    /// `new Worker(new URL(..))` without `{ type: "module" }` written out.
    ClassicWorker,
    /// Anything else.
    Other,
}

/// An emitted worker entry's file, and how code references it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct WorkerFile {
    file: String,
    /// Relative to the referencing chunk (`new URL(..)`), rather than by
    /// its twyla URL (`?worker`).
    relative: bool,
}

/// An import query: `import x from "./file?raw"`. One explicit match on the
/// name, here and in [`TwylaPlugin::load_query`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Query {
    /// The file's contents, as a string.
    Raw,
    /// The file's twyla URL; the file is emitted as with `asset.file`.
    Url,
    /// The file as a `data:` URL.
    Inline,
    /// A script bundled as a worker entry of its own: a function that
    /// starts it (`new Worker(url, {type: "module"})`).
    Worker,
    /// The same worker entry, but just its URL (`?worker&url`, the one
    /// combination of queries allowed).
    WorkerUrl,
}

impl Query {
    fn parse(query: &str) -> Result<Self, String> {
        match query {
            "raw" => Ok(Self::Raw),
            "url" => Ok(Self::Url),
            "inline" => Ok(Self::Inline),
            "worker" => Ok(Self::Worker),
            "worker&url" => Ok(Self::WorkerUrl),
            other => Err(format!(
                "unsupported import query `?{other}`: asset.js supports exactly one of `?raw`, \
                 `?url`, `?inline`, `?worker` and `?worker&url`"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Url => "url",
            Self::Inline => "inline",
            Self::Worker => "worker",
            Self::WorkerUrl => "worker&url",
        }
    }
}

/// The module id of a queried import: `\0` (rollup's "not a plain file"),
/// then the query, then the resolved path (relative to the root). Distinct from the plain import of
/// the same file (no prefix) and from other queries of it; equal for every
/// specifier that resolves to the same file with the same query.
const QUERY_ID_PREFIX: &str = "\0twyla-query:";

/// The module id `new URL("./w.ts", import.meta.url)` resolves to (then the
/// root-relative path): a stand-in whose load emits `w.ts` as a worker entry
/// chunk, which rolldown then rewrites the `new URL(..)` to point at.
const NEW_URL_ID_PREFIX: &str = "\0twyla-new-url:";

/// Whether `specifier` is root-absolute (`/scripts/x.ts`): relative to the
/// project root, as in typst and Vite, not the filesystem's. (`//host/x` is
/// a protocol-relative URL.)
fn is_root_absolute(specifier: &str) -> bool {
    specifier.starts_with('/') && !specifier.starts_with("//")
}

/// Whether `path` is a script a worker (or any entry) can be bundled from.
fn is_script(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(
        ext,
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx"
    )
}

/// Finds every `new URL("<literal>", import.meta.url)` in a module (the
/// pattern rolldown reports as a `NewUrl` import), and the call it's the
/// first argument of.
struct NewUrlFinder<'s> {
    source: &'s str,
    /// By the `new URL`'s span start.
    found: BTreeMap<u32, NewUrl>,
}

impl NewUrlFinder<'_> {
    fn line(&self, offset: u32) -> usize {
        let end = (offset as usize).min(self.source.len());
        self.source.as_bytes()[..end]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            + 1
    }
}

/// The literal of `new URL("<literal>", import.meta.url)`.
fn new_url_literal(expr: &oxc::ast::ast::NewExpression<'_>) -> Option<String> {
    use oxc::ast::ast::Expression;
    if !matches!(&expr.callee, Expression::Identifier(id) if id.name == "URL") {
        return None;
    }
    let is_import_meta_url = match expr.arguments.get(1).and_then(|a| a.as_expression()) {
        Some(Expression::StaticMemberExpression(member)) => {
            member.property.name == "url" && matches!(member.object, Expression::ImportMeta(_))
        }
        _ => false,
    };
    if !is_import_meta_url {
        return None;
    }
    match expr.arguments.first()?.as_expression()? {
        Expression::StringLiteral(lit) => Some(js_string(lit.value)),
        Expression::TemplateLiteral(tpl) if tpl.is_no_substitution_template() => {
            tpl.quasis[0].value.cooked.map(js_string)
        }
        _ => None,
    }
}

/// A JS string's value, as rolldown spells the import (lone surrogates
/// replaced).
fn js_string(value: oxc::str::JSStr<'_>) -> String {
    match value.as_str() {
        Some(value) => value.to_string(),
        None => char::decode_utf16(value.encode_utf16())
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect(),
    }
}

/// Whether `options` is an object literal with `type: "module"`.
fn is_module_worker_options(options: Option<&oxc::ast::ast::Argument<'_>>) -> bool {
    use oxc::ast::ast::{Expression, ObjectPropertyKind};
    let Some(Expression::ObjectExpression(object)) = options.and_then(|a| a.as_expression()) else {
        return false;
    };
    object.properties.iter().any(|property| match property {
        ObjectPropertyKind::ObjectProperty(p) => {
            !p.computed
                && p.key.static_name().is_some_and(|k| k == "type")
                && matches!(&p.value, Expression::StringLiteral(v) if v.value == "module")
        }
        ObjectPropertyKind::SpreadProperty(_) => false,
    })
}

impl<'a> oxc::ast_visit::Visit<'a> for NewUrlFinder<'_> {
    fn visit_new_expression(&mut self, it: &oxc::ast::ast::NewExpression<'a>) {
        use oxc::ast::ast::Expression;
        use oxc::span::GetSpan;
        let is_worker = matches!(&it.callee, Expression::Identifier(id)
            if id.name == "Worker" || id.name == "SharedWorker");
        if is_worker
            && let Some(Expression::NewExpression(url)) =
                it.arguments.first().and_then(|a| a.as_expression())
            && let Some(specifier) = new_url_literal(url)
        {
            let use_ = match is_module_worker_options(it.arguments.get(1)) {
                true => NewUrlUse::Worker,
                false => NewUrlUse::ClassicWorker,
            };
            let line = self.line(url.span().start);
            self.found.insert(
                url.span().start,
                NewUrl {
                    specifier,
                    line,
                    use_,
                },
            );
        } else if let Some(specifier) = new_url_literal(it) {
            let line = self.line(it.span().start);
            self.found.entry(it.span().start).or_insert(NewUrl {
                specifier,
                line,
                use_: NewUrlUse::Other,
            });
        }
        oxc::ast_visit::walk::walk_new_expression(self, it);
    }
}

/// A specifier's path and import query, if it has one. Specifiers that
/// start with a URL scheme (`https:`, `data:`, `node:`) are left alone; a
/// `:` later on is just part of a file name.
fn split_query(specifier: &str) -> Option<(&str, &str)> {
    let scheme = specifier.split_once(':').is_some_and(|(scheme, _)| {
        let mut chars = scheme.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    if scheme {
        return None;
    }
    specifier.split_once('?')
}

impl TwylaPlugin {
    /// The path a relative `specifier` names (`./logo.png` next to the
    /// importer), when it's the file the resolver resolved, through a
    /// symlink. `None` for anything else (a package, an alias, a probed
    /// extension, a `browser` field remap).
    fn lexical_target(
        &self,
        importer: Option<&str>,
        specifier: &str,
        resolved: &Path,
    ) -> Option<PathBuf> {
        let lexical = if is_root_absolute(specifier) {
            self.fs.admit(&self.ctx.root.join(&specifier[1..])).ok()?
        } else if specifier.starts_with("./") || specifier.starts_with("../") {
            let importer = Path::new(importer?);
            if !importer.is_absolute() {
                return None;
            }
            self.fs.admit(&importer.parent()?.join(specifier)).ok()?
        } else {
            return None;
        };
        if lexical == resolved {
            return None;
        }
        // Both admitted, so both inside the root.
        let same = std::fs::canonicalize(&lexical).ok()? == std::fs::canonicalize(resolved).ok()?;
        same.then_some(lexical)
    }

    /// Resolve `specifier` (no query) for `args`' importer and import kind
    /// with the bundle's own resolver — through the sandbox, with tsconfig
    /// `paths`, package `exports`, …, and recorded — mapping a root-absolute
    /// specifier to the project root. Not with `ctx.resolve`: that goes
    /// through the resolver rolldown's `BundleFactory` built on the OS
    /// filesystem, outside the sandbox. Returns the resolution and its
    /// (admitted) path.
    fn resolve_in_project(
        &self,
        args: &HookResolveIdArgs<'_>,
        specifier: &str,
    ) -> anyhow::Result<(rolldown_resolver::ResolveReturn, PathBuf)> {
        let mapped = match is_root_absolute(specifier) {
            true => {
                // `/../x` names a file above the root; refused by name
                // before the resolver goes looking.
                let mut depth = 0usize;
                for part in specifier.split('/') {
                    match part {
                        "" | "." => {}
                        ".." if depth == 0 => {
                            return Err(self.refuse(format!(
                                "can't resolve `{}`: it's outside the twyla project root",
                                args.specifier
                            )));
                        }
                        ".." => depth -= 1,
                        _ => depth += 1,
                    }
                }
                self.ctx
                    .root
                    .join(&specifier[1..])
                    .to_string_lossy()
                    .into_owned()
            }
            false => specifier.to_string(),
        };
        let resolved = self
            .resolver
            .resolve(args.importer.map(Path::new), &mapped, args.kind, false)
            .map_err(|err| self.refuse(format!("can't resolve `{}`: {err}", args.specifier)))?;
        let path = Path::new(resolved.path.as_str());
        if !path.is_absolute() {
            return Err(self.refuse(format!(
                "can't import `{}`: it isn't a file in the project",
                args.specifier
            )));
        }
        // Confined like every other access (the load reads through the
        // sandbox too); refused here so the error names the import.
        let Ok(admitted) = self.fs.admit(path) else {
            return Err(self.refuse(format!(
                "can't resolve `{}`: it's outside the twyla project root",
                args.specifier
            )));
        };
        Ok((resolved, admitted))
    }

    /// `path` (inside the root) as it appears in a module id: root-relative,
    /// since rolldown prints `\0` ids verbatim (module comments, sourcemap
    /// sources), and the output mustn't depend on where the project is.
    fn root_relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.ctx.root)
            .expect("admitted paths are inside the root")
            .to_string_lossy()
            .into_owned()
    }

    /// `new URL("<literal>", import.meta.url)`, which rolldown reports as an
    /// import (`experimental.resolve_new_url_to_asset`). As the first
    /// argument of `new Worker(.., { type: "module" })`, a local script is
    /// bundled as a worker entry (as with `?worker`). Any other local
    /// `new URL` is an error: the URL would resolve next to the bundled
    /// chunk in `assets/`, where the file isn't. A URL with a scheme (or
    /// `//host/…`) is left alone.
    fn resolve_new_url(&self, args: &HookResolveIdArgs<'_>) -> HookResolveIdReturn {
        let specifier = args.specifier;
        let scheme = split_query(specifier).is_none() && specifier.contains(':');
        if scheme || specifier.starts_with("//") {
            return Ok(Some(HookResolveIdOutput {
                id: specifier.into(),
                external: Some(ResolvedExternal::Bool(true)),
                ..Default::default()
            }));
        }
        // How the module uses it, from its parse. Each use of the literal
        // in the module must be a module worker.
        let uses: Vec<NewUrl> = args
            .importer
            .and_then(|importer| self.new_urls.lock().unwrap().get(importer).cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(|u| u.specifier == specifier)
            .collect();
        let shown = args
            .importer
            .map(|i| self.shown(Path::new(i)))
            .unwrap_or_else(|| "an entry".to_string());
        let at = |line: usize| format!("{shown}:{line}");
        let not_a_worker = uses.iter().find(|u| u.use_ == NewUrlUse::Other);
        if uses.is_empty() || not_a_worker.is_some() {
            let at = not_a_worker.map_or(shown.clone(), |u| at(u.line));
            return Err(self.refuse(format!(
                "{at}: `new URL(\"{specifier}\", import.meta.url)` would resolve next to the \
                 bundled script in `assets/`, where `{specifier}` isn't emitted; import its URL \
                 with `import url from \"{specifier}?url\"` instead (or, for a worker, \
                 `new Worker(new URL(\"{specifier}\", import.meta.url), {{ type: \"module\" }})`)"
            )));
        }
        if let Some(classic) = uses.iter().find(|u| u.use_ == NewUrlUse::ClassicWorker) {
            return Err(self.refuse(format!(
                "{}: the worker `{specifier}` is bundled as a module, so it must be started as \
                 one: `new Worker(new URL(\"{specifier}\", import.meta.url), \
                 {{ type: \"module\" }})`, with the options written out",
                at(classic.line)
            )));
        }
        let line = uses[0].line;
        let plain = split_query(specifier).map_or(specifier, |(path, _)| path);
        if plain != specifier || specifier.contains('#') {
            return Err(self.refuse(format!(
                "{}: a worker's `new URL(\"{specifier}\", import.meta.url)` can't have a query \
                 or a fragment",
                at(line)
            )));
        }
        if !self.split {
            return Err(self.refuse(format!(
                "{}: the worker `{specifier}` is bundled as a file of its own, which takes code \
                 splitting (`split: true`, `format: \"esm\"`)",
                at(line)
            )));
        }
        let (_, admitted) = self.resolve_in_project(args, specifier)?;
        if !is_script(&admitted) {
            return Err(self.refuse(format!(
                "{}: the worker `{specifier}` must be a JavaScript or TypeScript module",
                at(line)
            )));
        }
        Ok(Some(HookResolveIdOutput {
            id: format!("{NEW_URL_ID_PREFIX}{}", self.root_relative(&admitted)).into(),
            ..Default::default()
        }))
    }

    /// `path` for messages: root-relative when inside the root.
    fn shown(&self, path: &Path) -> String {
        path.strip_prefix(&self.ctx.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// Refuse an import: rolldown's diagnostic drops the reason, so it's
    /// recorded for the build to report.
    fn refuse(&self, message: String) -> anyhow::Error {
        self.refused.lock().unwrap().push(message.clone());
        anyhow::anyhow!(message)
    }

    /// The JS module a queried import loads as.
    fn load_query(&self, id: &str, query: Query, path: &Path) -> Result<String, String> {
        let shown = path
            .strip_prefix(&self.ctx.root)
            .unwrap_or(path)
            .display()
            .to_string();
        let read = || {
            OxcFileSystem::read(&self.fs, path)
                .map_err(|err| format!("can't read `{shown}?{}`: {err}", query.name()))
        };
        let value = match query {
            Query::Raw => String::from_utf8(read()?).map_err(|_| {
                format!("`{shown}?raw` isn't valid UTF-8; import it with `?url` or `?inline`")
            })?,
            Query::Inline => {
                // Written straight into the module: a mime type and base64
                // need no escaping in a JS string.
                use base64::Engine as _;
                let bytes = read()?;
                let mime = crate::serve::mime_for(path).replace(' ', "");
                let mut code = String::with_capacity(bytes.len() / 3 * 4 + mime.len() + 64);
                code.push_str("export default \"data:");
                code.push_str(&mime);
                code.push_str(";base64,");
                base64::engine::general_purpose::STANDARD.encode_string(&bytes, &mut code);
                code.push_str("\";\n");
                return Ok(code);
            }
            Query::Url => {
                // Streamed, never held in memory, like `asset.file`.
                let admitted = self
                    .fs
                    .admit(path)
                    .map_err(|err| format!("can't read `{shown}?url`: {err}"))?;
                let hash = self
                    .fs
                    .rec
                    .open(&admitted)
                    .and_then(|file| file.finish())
                    .map_err(|err| format!("can't read `{shown}?url`: {err}"))?;
                let built = super::file::copied(admitted.clone(), hash);
                let output_path = self.ctx.default_asset_output(&built);
                let url = self.ctx.asset_url(&output_path);
                self.urls.lock().unwrap().insert(
                    id.to_string(),
                    UrlFile {
                        output_path,
                        source: admitted,
                        sha256: hash,
                    },
                );
                url
            }
            Query::Worker | Query::WorkerUrl => unreachable!("loaded in `load`, which emits"),
        };
        let json = serde_json::to_string(&value).expect("a string serializes");
        Ok(format!("export default {json};\n"))
    }
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
        HookUsage::ResolveId
            | HookUsage::Load
            | HookUsage::TransformAst
            | HookUsage::ResolveFileUrl
            | HookUsage::GenerateBundle
    }

    async fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        if self.modules.contains_key(args.specifier) {
            return Ok(Some(HookResolveIdOutput {
                // The `\0` prefix is the rollup convention for "not a file".
                id: format!("\0{}", args.specifier).into(),
                ..Default::default()
            }));
        }
        // Entries are the absolute paths twyla passes in.
        if args.is_entry {
            return Ok(None);
        }
        if matches!(args.kind, ImportKind::NewUrl) {
            return self.resolve_new_url(args);
        }
        let Some((bare, query)) = split_query(args.specifier) else {
            if !is_root_absolute(args.specifier) {
                return Ok(None);
            }
            // A plain root-absolute import: resolved here, since rolldown
            // would take it as a filesystem path.
            let (resolved, _) = self.resolve_in_project(args, args.specifier)?;
            return Ok(Some(HookResolveIdOutput::from_resolved_id(
                ResolvedId::from(resolved),
            )));
        };
        // A leading `#` is a package import (`#internal/x.txt`); anywhere
        // else it'd be a fragment, which a queried import has no use for (and
        // which the resolver would split off the path behind our back).
        if bare.chars().skip(1).any(|c| c == '#') || query.contains('#') {
            return Err(self.refuse(format!(
                "can't import `{}`: an import with a query can't have a `#` fragment",
                args.specifier
            )));
        }
        let query = Query::parse(query)
            .map_err(|err| self.refuse(format!("can't import `{}`: {err}", args.specifier)))?;
        let (_, admitted) = self.resolve_in_project(args, bare)?;
        if matches!(query, Query::Worker | Query::WorkerUrl) {
            if !is_script(&admitted) {
                return Err(self.refuse(format!(
                    "can't import `{}`: a worker must be a JavaScript or TypeScript module",
                    args.specifier
                )));
            }
            if !self.split {
                return Err(self.refuse(format!(
                    "can't import `{}`: a worker is bundled as a file of its own, which takes \
                     code splitting (`split: true`, `format: \"esm\"`)",
                    args.specifier
                )));
            }
        }
        // The resolver follows symlinks to the real file. A relative import
        // of a link names the file by the link instead, as `asset.file`
        // does: the same file, under the name the code (and the page) uses.
        // (A worker is bundled from the real file, like any entry.)
        let named = match query {
            Query::Worker | Query::WorkerUrl => admitted,
            _ => self
                .lexical_target(args.importer, bare, &admitted)
                .unwrap_or(admitted),
        };
        Ok(Some(HookResolveIdOutput {
            id: format!(
                "{QUERY_ID_PREFIX}{}:{}",
                query.name(),
                self.root_relative(&named)
            )
            .into(),
            ..Default::default()
        }))
    }

    async fn load(&self, ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        // `new URL("./w.ts", import.meta.url)`: emit the worker entry and tie
        // it to this stand-in module, so rolldown rewrites the `new URL(..)`
        // to the chunk's file. The stand-in itself never runs.
        if let Some(path) = args.id.strip_prefix(NEW_URL_ID_PREFIX) {
            let reference = self.emit_worker(&ctx, &self.ctx.root.join(path), true)?;
            ctx.associate_module_with_file_ref(args.id, &reference);
            return Ok(Some(HookLoadOutput {
                code: ArcStr::from("export {};\n"),
                module_type: Some(ModuleType::Js),
                ..Default::default()
            }));
        }
        if let Some(rest) = args.id.strip_prefix(QUERY_ID_PREFIX)
            && let Some((name, path)) = rest.split_once(':')
        {
            let query = Query::parse(name).map_err(|err| self.refuse(err))?;
            if matches!(query, Query::Worker | Query::WorkerUrl) {
                let worker = self.ctx.root.join(path);
                let reference = self.emit_worker(&ctx, &worker, false)?;
                // The URL is filled in by `resolve_file_url`, once the worker
                // chunk has its name.
                let url = format!("import.meta.ROLLDOWN_FILE_URL_{reference}");
                let code = match query {
                    Query::Worker => format!(
                        "export default function WorkerWrapper(options) {{\n  \
                         return new Worker({url}, {{ type: \"module\", ...options }});\n}}\n"
                    ),
                    _ => format!("export default {url};\n"),
                };
                return Ok(Some(HookLoadOutput {
                    code: ArcStr::from(code),
                    module_type: Some(ModuleType::Js),
                    ..Default::default()
                }));
            }
            let code = self
                .load_query(args.id, query, &self.ctx.root.join(path))
                .map_err(|err| self.refuse(err))?;
            return Ok(Some(HookLoadOutput {
                code: ArcStr::from(code),
                module_type: Some(ModuleType::Js),
                ..Default::default()
            }));
        }
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
                 text modules; import it with `?raw` (its contents as a string), `?url` (its \
                 URL; the file is emitted) or `?inline` (a `data:` URL)"
            );
            return Err(self.refuse(message));
        }
        Ok(None)
    }

    /// Record the module's `new URL("<literal>", import.meta.url)`s, with
    /// the call each is in, for `resolve_new_url`.
    async fn transform_ast(
        &self,
        _ctx: &PluginContext,
        args: HookTransformAstArgs<'_>,
    ) -> HookTransformAstReturn {
        use oxc::ast_visit::Visit as _;
        let mut finder = NewUrlFinder {
            source: args.ast.source(),
            found: BTreeMap::new(),
        };
        finder.visit_program(args.ast.program());
        if !finder.found.is_empty() {
            self.new_urls
                .lock()
                .unwrap()
                .insert(args.id.to_string(), finder.found.into_values().collect());
        }
        Ok(args.ast)
    }

    /// Name every emitted worker's file, now that names are final.
    async fn generate_bundle(
        &self,
        ctx: &PluginContext,
        _args: &mut HookGenerateBundleArgs<'_>,
    ) -> HookNoopReturn {
        let mut files = Vec::new();
        for (reference, relative) in self.emitted.lock().unwrap().iter() {
            files.push(WorkerFile {
                file: ctx.get_file_name(reference)?.to_string(),
                relative: *relative,
            });
        }
        files.sort();
        files.dedup();
        *self.worker_files.lock().unwrap() = files;
        Ok(())
    }

    /// A worker chunk's URL: root-absolute (with twyla's base URL), like
    /// every other twyla URL, so it doesn't depend on where the importing
    /// chunk is, or on `import.meta.url` (which a worker started from an
    /// inlined script wouldn't have).
    async fn resolve_file_url(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveFileUrlArgs<'_>,
    ) -> HookResolveFileUrlReturn {
        let url = self.ctx.asset_url(&format!("assets/{}", args.file_name));
        Ok(Some(
            serde_json::to_string(&url).expect("a string serializes"),
        ))
    }
}

impl TwylaPlugin {
    /// Emit `worker` (absolute, admitted) as an entry chunk of this build,
    /// referenced `relative`ly or not.
    fn emit_worker(
        &self,
        ctx: &SharedLoadPluginContext,
        worker: &Path,
        relative: bool,
    ) -> anyhow::Result<ArcStr> {
        let reference = ctx
            .emit_chunk(EmittedChunk {
                id: worker.to_string_lossy().into_owned(),
                name: None,
                file_name: None,
                importer: None,
                preserve_entry_signatures: None,
            })
            .map_err(|err| {
                self.refuse(format!(
                    "can't bundle the worker {}: {err}",
                    self.shown(worker)
                ))
            })?;
        self.emitted
            .lock()
            .unwrap()
            .push((reference.clone(), relative));
        Ok(reference)
    }
}

#[cfg(test)]
mod tests;

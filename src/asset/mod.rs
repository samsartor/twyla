// `AssetSpec::Raw` carries `Bytes`, whose `Arc` refcount reads to clippy as
// interior mutability — but the spec's `Hash`/`Eq` are purely content-based, so
// it's a sound `HashMap` key (the same reason typst hashes `Bytes` freely).
#![allow(clippy::mutable_key_type)]

/// Generate the shared `.url()` / `.read()` contextual scope methods for an
/// asset element. Every asset element behaves identically bar *how its spec is
/// built*, so each module supplies only that: `$spec` is its
/// `fn(&Packed<Elem>, StyleChain) -> HintedStrResult<AssetSpec>` (which folds in
/// `#set` styles), and `$default_encoding` is its `.read()` default (`Some(..)`
/// for text-ish assets, `None` for binary like images).
///
/// Both methods build the spec, then resolve it through the introspector —
/// `.url()` to a fingerprinted URL, `.read()` to the asset's bytes. They are
/// contextual so they run during realization, when the introspector (and so the
/// resolved asset) exists, rather than eagerly at eval. Types are fully
/// qualified so the macro needs nothing imported at its call site beyond the
/// `func`/`scope` attributes the element already uses.
macro_rules! asset_methods {
    ($this:ty, $spec:path, $default_encoding:expr) => {
        #[scope]
        impl $this {
            /// The resolved, fingerprinted URL of this asset. Contextual — call
            /// it inside `#context`.
            ///
            /// `engine` precedes the `this` self-positional: the `#[func]` macro
            /// forwards special params ahead of ordinary positionals, and the
            /// method call prepends the element as that positional.
            #[func(contextual)]
            fn url(
                engine: &mut ::typst::engine::Engine,
                context: ::comemo::Tracked<::typst::foundations::Context>,
                this: ::typst::foundations::Content,
            ) -> ::typst::diag::HintedStrResult<::typst::foundations::Str> {
                let elem = this.into_packed::<$this>().unwrap();
                let spec = $spec(&elem, context.styles()?)?;
                Ok($crate::asset::resolve_or_request(engine, spec, elem.span()))
            }

            /// This asset's bytes — for inlining instead of linking. Mirrors the
            /// native `read`: UTF-8 `str` by default, raw `bytes` with
            /// `encoding: none`. Contextual.
            #[func(contextual)]
            fn read(
                engine: &mut ::typst::engine::Engine,
                context: ::comemo::Tracked<::typst::foundations::Context>,
                this: ::typst::foundations::Content,
                /// The encoding to read the asset with. If `{none}`, returns raw
                /// bytes; otherwise the bytes are decoded as UTF-8 into a string.
                #[named]
                #[default($default_encoding)]
                encoding: Option<::typst::loading::Encoding>,
            ) -> ::typst::diag::HintedStrResult<::typst::loading::Readable> {
                let elem = this.into_packed::<$this>().unwrap();
                let spec = $spec(&elem, context.styles()?)?;
                $crate::asset::read_or_request(engine, spec, elem.span(), encoding)
            }
        }
    };
}

pub(crate) mod file;
pub(crate) mod image;
pub(crate) mod raw;
pub(crate) mod sass;
pub(crate) mod typst_doc;

use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::path::Path;

use comemo::Tracked;
use ecow::{EcoString, eco_format, eco_vec};
use iddqd::IdHashItem;
use typst::diag::{HintedStrResult, SourceDiagnostic, SourceResult};
use typst::engine::Engine;
use typst::foundations::{
    Binding, Bytes, Content, Module, PathOrStr, Repr, Scope, Str, Value, ty,
};
use typst::introspection::{History, Introspect, Introspector};
use typst::loading::{Encoding, Readable};
use typst::syntax::{FileId, Span};
use typst_utils::hash128;

use crate::render::Emit;
use crate::resolver::Upstream;

// ---------------------------------------------------------------------------
// Keys: AssetSpec -> AssetRequest -> Asset
// ---------------------------------------------------------------------------

/// What produces an asset's bytes — the cache/store key. An asset's output is
/// a pure function of its spec, so this is what every map keys on.
#[derive(Clone, PartialEq, Hash, Debug)]
pub enum AssetSpec {
    File {
        file: FileId,
    },
    Sass {
        file: FileId,
        minify: bool,
    },
    Raw {
        bytes: Bytes,
        /// Output extension (drives the static server's Content-Type), sniffed
        /// by the caller; `None` → `bin`.
        ext: Option<EcoString>,
    },
    Image {
        source: ImageSource,
        width: Option<u32>,
        height: Option<u32>,
        fit: image::Fit,
        filter: image::Filter,
        /// `None` keeps the source format.
        format: Option<image::Format>,
        quality: u8,
    },
    Typst {
        input: typst_doc::TypstInput,
        format: typst_doc::Format,
        /// `png` resolution in pixels per inch. Normalized to `0` for the other
        /// formats, so it never fragments their output.
        ppi: i64,
    },
}

/// Where a processed image's source bytes come from.
#[derive(Clone, PartialEq, Hash, Debug)]
pub enum ImageSource {
    /// A project file, read from disk (and watched). Fingerprinted by content.
    File(FileId),
    /// In-memory bytes (an inline image), content-addressed by the bytes.
    Bytes(Bytes),
}

impl Eq for AssetSpec {}

#[derive(Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum OutputReq {
    /// The user wants a URL, but any URL will do.
    Url,
    /// The user wants to read the asset bytes directly.
    Read,
}

/// A spec plus the call site that requested it, and its future output policy.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct AssetReq {
    pub spec: AssetSpec,
    ///  carried so a failed [`build`](file::build) (missing file, sass error)
    /// can blame a real source location instead of `<detached>`
    pub span: Span,
    /// How the asset is going to be used.
    pub outputs: BTreeSet<OutputReq>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct AssetReqIntrospect(pub AssetReq);

pub fn hash_spec(spec: &AssetSpec) -> u128 {
    hash128(&("__twyla_asset__", &spec))
}

impl Introspect for AssetReqIntrospect {
    type Output = Option<ResolvedAsset>;

    fn introspect(
        &self,
        _engine: &mut typst::engine::Engine,
        introspector: Tracked<dyn Introspector + '_>,
    ) -> Self::Output {
        let Some(Value::Dyn(value)) = Introspector::value(&*introspector, hash_spec(&self.0.spec))
        else {
            return None;
        };
        value.downcast().cloned()
    }

    fn diagnose(&self, _history: &History<Self::Output>) -> SourceDiagnostic {
        // An asset's output is a pure function of its spec, so the realistic
        // cause of non-convergence is a generator that derives the *spec* from
        // something unstable (e.g. an asset whose contents depend on a counter).
        SourceDiagnostic::warning(self.0.span, "this asset did not stabilize").with_hint(
            "an asset's output should be a pure function of its source; resolving one whose \
             inputs keep changing each pass cannot converge",
        )
    }
}

// Assets are *elements* ([`file::FileAsset`], [`sass::SassAsset`]), not a
// single handle type. Each element captures its source fields (so
// `#set asset.sass(minify: false)` works) and exposes the same two contextual
// scope methods — `.url()` and `.read()` — that build an [`AssetSpec`] from
// those fields and resolve it through the introspector. The element→spec→resolve
// machinery they share lives here: [`resolve_or_request`] / [`read_or_request`]
// (the introspect-or-request protocol) and [`show_unresolved`] (the default
// show rule, which refuses to render a bare asset). Discovery is *lazy*: it
// happens only when `.url()`/`.read()` actually run, so an asset that's never
// used is never built. See the module docs and
// [[twyla_element_scope_methods_spike]].

/// The resolve-or-request protocol every `asset.*` consumer obeys, in one
/// place: [`introspect`](Engine::introspect) the spec through the resolver-backed
/// introspector ([`AssetReqIntrospect`]). A **hit** returns the built
/// [`ResolvedAsset`]; a **miss** returns `None` *and* records the request, so
/// the compile loop builds it and a later iteration re-runs this against the
/// now-populated introspector (that re-run is what makes the page converge).
///
/// `outputs` records how the caller intends to use the asset (a URL vs raw
/// bytes) — carried for emission policy and the upcoming per-request output
/// paths. `span` blames a real source location when a [`build`](file::build)
/// fails (missing file, sass error).
fn introspect_asset(
    engine: &mut Engine,
    spec: AssetSpec,
    span: Span,
    outputs: BTreeSet<OutputReq>,
) -> Option<ResolvedAsset> {
    engine.introspect(AssetReqIntrospect(AssetReq { spec, span, outputs }))
}

/// The set for a caller that wants a URL.
fn want_url() -> BTreeSet<OutputReq> {
    BTreeSet::from([OutputReq::Url])
}

/// Resolve a spec's URL, or request it (returns [`ASSET_PENDING`] on a miss).
/// Shared by the asset elements' `.url()` and the native image rule.
pub(crate) fn resolve_or_request(engine: &mut Engine, spec: AssetSpec, span: Span) -> Str {
    match introspect_asset(engine, spec, span, want_url()) {
        Some(asset) => Str::from(asset.url.as_str()),
        None => Str::from(ASSET_PENDING),
    }
}

/// A resolved image: its URL plus the output's intrinsic pixel dimensions (when
/// known). Returned by [`resolve_image_or_request`] for the native image rule.
pub(crate) struct ResolvedImage {
    pub url: Str,
    pub dimensions: Option<(u32, u32)>,
}

/// Resolve a raster-image spec to its URL *and* output dimensions, or request it
/// on a miss. Like [`resolve_or_request`] but also surfaces `built.dimensions`
/// so the native rule can emit `<img width height>`.
pub(crate) fn resolve_image_or_request(
    engine: &mut Engine,
    spec: AssetSpec,
    span: Span,
) -> ResolvedImage {
    match introspect_asset(engine, spec, span, want_url()) {
        Some(asset) => ResolvedImage {
            url: Str::from(asset.url.as_str()),
            dimensions: asset.built.dimensions,
        },
        None => ResolvedImage {
            url: Str::from(ASSET_PENDING),
            dimensions: None,
        },
    }
}

/// Resolve a spec's bytes, or request it (returns an empty value on a miss — the
/// placeholder, discarded before convergence). Backs the elements' `.read()`;
/// `encoding` selects `str` (UTF-8) vs raw `bytes`, mirroring the native `read`.
/// The bytes come from `built.emit`, so a path-backed asset is read from disk
/// here rather than held in RAM.
fn read_or_request(
    engine: &mut Engine,
    spec: AssetSpec,
    span: Span,
    encoding: Option<Encoding>,
) -> HintedStrResult<Readable> {
    match introspect_asset(engine, spec, span, BTreeSet::from([OutputReq::Read])) {
        Some(asset) => {
            let bytes = asset
                .built
                .emit
                .read()
                .map_err(|err| eco_format!("failed to read asset bytes: {err}"))?;
            decode(bytes, encoding)
        }
        None => Ok(decode_empty(encoding)),
    }
}

/// Apply `read`'s `encoding` to resolved bytes: `none` → raw bytes, UTF-8 →
/// decode to a string (erroring on invalid UTF-8, like the native `read`).
fn decode(bytes: Bytes, encoding: Option<Encoding>) -> HintedStrResult<Readable> {
    match encoding {
        None => Ok(Readable::Bytes(bytes)),
        Some(Encoding::Utf8) => {
            Ok(Readable::Str(bytes.to_str().map_err(|err| {
                eco_format!("asset is not valid UTF-8: {err}")
            })?))
        }
    }
}

/// The empty placeholder returned on a miss, of the type `encoding` selects.
fn decode_empty(encoding: Option<Encoding>) -> Readable {
    match encoding {
        None => Readable::Bytes(Bytes::new(Vec::<u8>::new())),
        Some(Encoding::Utf8) => Readable::Str(Str::from("")),
    }
}

/// Placeholder URL returned for an as-yet-unresolved asset. By convergence
/// every consumer has re-run against a populated map, so this never survives
/// into output (a leak means non-convergence — a bug).
const ASSET_PENDING: &str = "/__twyla-asset-pending__";

/// Resolve an asset element's `path` field to the `FileId` it names, relative
/// to `span`'s file (the file the asset call was written in — mirrors how
/// `read`/`image` resolve their paths). Shared by the element `.url()`/`.read()`
/// methods, which resolve lazily off the element's own span.
pub(crate) fn resolve_path(path: &PathOrStr, span: Span) -> HintedStrResult<FileId> {
    Ok(path.resolve_if_some(span.id())?.intern())
}

/// Default show for a bare asset element: refuse to render. An asset exists to
/// hand back a `url`/bytes; one that reaches realization was never resolved
/// (`.url()`/`.read()` consume the element before it can be shown), so there is
/// nothing meaningful to render. Per-element `ShowFn`s delegate here.
pub(crate) fn show_unresolved(span: Span, name: &str) -> SourceResult<Content> {
    Err(eco_vec![
        SourceDiagnostic::error(span, eco_format!("`asset.{name}` cannot be shown directly"))
            .with_hint(eco_format!(
                "resolve it inside a `#context` block — `.url()` to link it, `.read()` to inline it"
            ))
    ])
}

/// Build the `asset` module (`asset.file`, `asset.sass`) for the global scope.
/// Each name binds an *element* so `#set asset.sass(..)` works (see
/// [`file::FileAsset`] / [`sass::SassAsset`]).
pub fn module() -> Module {
    let mut scope = Scope::new();
    scope.define_elem::<file::FileAsset>();
    scope.define_elem::<sass::SassAsset>();
    scope.define_elem::<image::ImageAsset>();
    scope.define_elem::<typst_doc::TypstAsset>();
    Module::new("asset", scope)
}

/// Bind the `asset` module into a global scope. Called from [`crate::prelude`].
pub fn install(global: &mut Scope) {
    global.bind("asset".into(), Binding::detached(Value::Module(module())));
}

// ---------------------------------------------------------------------------
// Per-type build output + resolved record
// ---------------------------------------------------------------------------

/// What a per-type `build` ([`file::build`], [`sass::build`]) produces. The
/// resolver turns it into a [`ResolvedAsset`] by adding the fingerprinted name,
/// output path, and URL (all shared logic).
#[derive(Clone, Debug, PartialEq)]
pub struct Built {
    /// How the bytes reach the output.
    pub emit: Emit,
    /// Every on-disk file the build read (entry + transitive imports). Drives
    /// invalidation and the watcher's dependency set.
    pub upstream: Vec<Upstream>,
    /// Content hash of the *output* bytes (the fingerprint).
    pub content_hash: u128,
    /// Output extension (`css` for sass, the source ext for a copy)
    pub ext: Option<String>,
    /// The name of the original asset file, if any.
    pub stem: Option<String>,
    /// Intrinsic pixel dimensions of the output, when it's a raster image
    /// ([`image::build`]) — lets the native image rule emit `<img width height>`
    /// for aspect-ratio reservation. `None` for non-image assets.
    pub dimensions: Option<(u32, u32)>,
}

impl Hash for Built {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.content_hash.hash(state);
        self.stem.hash(state);
        if let Emit::Copy(path) = &self.emit {
            path.hash(state);
        }
    }
}

/// One processed asset: where it lives, how to emit it, and what it depends on.
///
/// This is the value carried on the style chain (inside [`ResolvedAssets`]), so
/// `asset.*().url()` / `.read()` read straight off it. Its `built.emit` is
/// either in-memory bytes (Arc-shared, cheap to clone onto the chain) or a path
/// the bytes are read from on demand — so injecting the whole record never pulls
/// asset bytes into RAM.
#[ty]
#[derive(Clone, Debug, PartialEq, Hash)]
pub struct ResolvedAsset {
    /// The spec that produced it (its key).
    pub spec: AssetSpec,
    /// The built asset
    pub built: Built,
    /// Bundle-relative output path, e.g. `assets/main-<hash>.css`.
    pub output_path: String,
    /// The public, root-relative URL `.url()` returns (`output_path` resolved
    /// through [`TwylaContext::asset_url`], so it folds in any `base_url`).
    pub url: String,
}

impl Repr for ResolvedAsset {
    fn repr(&self) -> EcoString {
        todo!()
    }
}

impl IdHashItem for ResolvedAsset {
    type Key<'a> = &'a AssetSpec;

    fn key(&self) -> &AssetSpec {
        &self.spec
    }

    iddqd::id_upcast!();
}

impl ResolvedAsset {
    /// The on-disk source files this asset depends on — for the serve watcher.
    pub fn upstream_paths(&self) -> impl Iterator<Item = &Path> {
        self.built.upstream.iter().map(|u| u.path.as_path())
    }
}

#[cfg(test)]
mod tests;

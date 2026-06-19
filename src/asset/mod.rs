// `AssetSpec::Raw` carries `Bytes`, whose `Arc` refcount reads to clippy as
// interior mutability — but the spec's `Hash`/`Eq` are purely content-based, so
// it's a sound `HashMap` key (the same reason typst hashes `Bytes` freely).
#![allow(clippy::mutable_key_type)]

/// Generate the shared `.url()` / `.read()` contextual scope methods for an
/// asset element. Every asset element behaves identically bar *how its spec is
/// built* and how its `output` field is read, so each module supplies those:
/// `$spec` is its `fn(&Packed<Elem>, StyleChain) -> HintedStrResult<AssetSpec>`
/// (which folds in `#set` styles), `$output` is its output-field accessor, and
/// `$default_encoding` is its `.read()` default (`Some(..)`
/// for text-ish assets, `None` for binary like images).
///
/// Both methods build the spec, then resolve it through the introspector —
/// `.url()` to a fingerprinted URL, `.read()` to the asset's bytes. They are
/// contextual so they run during realization, when the introspector (and so the
/// resolved asset) exists, rather than eagerly at eval. Types are fully
/// qualified so the macro needs nothing imported at its call site beyond the
/// `func`/`scope` attributes the element already uses.
macro_rules! asset_methods {
    ($this:ty, $spec:path, $output:path, $default_encoding:expr) => {
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
                let styles = context.styles()?;
                let spec = $spec(&elem, styles)?;
                let output = $output(&elem, styles)?;
                Ok($crate::asset::resolve_or_request(
                    engine,
                    spec,
                    elem.span(),
                    output,
                ))
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

use std::hash::{Hash, Hasher};
use std::path::Path;

use comemo::Tracked;
use ecow::{EcoString, EcoVec, eco_format, eco_vec};
use iddqd::IdHashItem;
use sha2::{Digest, Sha256};
use typst::diag::{HintedStrResult, HintedString, SourceDiagnostic, SourceResult};
use typst::engine::Engine;
use typst::foundations::{
    AutoValue, Binding, Bytes, Content, Func, Module, PathOrStr, Repr, Scope, Str, Value, cast, ty,
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

#[derive(Clone, PartialEq, Hash, Debug)]
pub enum OutputReq {
    /// The user wants to read the asset bytes directly.
    Read,
    /// The user wants a URL/emission path, using the default/reuse policy.
    Auto,
    /// The user wants a specific bundle-relative output path.
    Fixed(EcoString),
    /// The user derives a bundle-relative output path from hash/ext/stem.
    Derive(Func),
}

cast! {
    OutputReq,
    self => match self {
        Self::Read => Value::None,
        Self::Auto => AutoValue.into_value(),
        Self::Fixed(v) => v.into_value(),
        Self::Derive(v) => v.into_value(),
    },
    v: AutoValue => {
        let _ = v;
        Self::Auto
    },
    v: EcoString => Self::Fixed(v),
    v: Func => Self::Derive(v),
}

/// A spec plus the call site that requested it, and its future output policy.
#[derive(Clone, PartialEq, Hash, Debug)]
pub struct AssetReq {
    pub spec: AssetSpec,
    ///  carried so a failed [`build`](file::build) (missing file, sass error)
    /// can blame a real source location instead of `<detached>`
    pub span: Span,
    /// How the asset is going to be used.
    pub outputs: Vec<OutputReq>,
}

#[derive(Clone, PartialEq, Hash, Debug)]
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
// (the introspect-or-request protocol) and [`emit_or_request`] (the default
// show rule, which emits and vanishes for a bare asset). Discovery is *lazy*: it
// happens only when `.url()`/`.read()` or a bare shown asset actually runs, so an
// asset that's never used is never built. See the module docs and
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
    outputs: Vec<OutputReq>,
) -> Option<ResolvedAsset> {
    engine.introspect(AssetReqIntrospect(AssetReq {
        spec,
        span,
        outputs,
    }))
}

/// Resolve a spec's URL, or request it (returns [`ASSET_PENDING`] on a miss).
/// Shared by the asset elements' `.url()` and the native image rule.
pub(crate) fn resolve_or_request(
    engine: &mut Engine,
    spec: AssetSpec,
    span: Span,
    output: OutputReq,
) -> Str {
    match introspect_asset(engine, spec, span, vec![output.clone()]) {
        Some(asset) => Str::from(asset.url_for(&output)),
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
    match introspect_asset(engine, spec, span, vec![OutputReq::Auto]) {
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

/// Request emission of a bare asset element and render nothing.
pub(crate) fn emit_or_request(
    engine: &mut Engine,
    spec: HintedStrResult<AssetSpec>,
    span: Span,
    output: HintedStrResult<OutputReq>,
) -> SourceResult<Content> {
    let spec = spec.map_err(|err| hinted_error(span, err))?;
    let output = output.map_err(|err| hinted_error(span, err))?;
    let _ = introspect_asset(engine, spec, span, vec![output]);
    Ok(Content::empty())
}

fn hinted_error(span: Span, err: HintedString) -> EcoVec<SourceDiagnostic> {
    let mut diag = SourceDiagnostic::error(span, err.message().clone());
    for hint in err.hints() {
        diag = diag.with_hint(hint.clone());
    }
    eco_vec![diag]
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
    match introspect_asset(engine, spec, span, vec![OutputReq::Read]) {
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

pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

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
    /// SHA-256 of the *output* bytes (the fingerprint).
    pub sha256: [u8; 32],
    /// Output extension (`css` for sass, the source ext for a copy)
    pub ext: Option<String>,
    /// The name of the original asset file, if any.
    pub stem: Option<String>,
    /// Intrinsic pixel dimensions of the output, when it's a raster image
    /// ([`image::build`]) — lets the native image rule emit `<img width height>`
    /// for aspect-ratio reservation. `None` for non-image assets.
    pub dimensions: Option<(u32, u32)>,
}

impl Built {
    /// Full lowercase hex SHA-256 of the emitted bytes.
    pub fn sha256_hex(&self) -> EcoString {
        EcoString::from(hex::encode(self.sha256))
    }
}

impl Hash for Built {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.sha256.hash(state);
        self.stem.hash(state);
        if let Emit::Copy(path) = &self.emit {
            path.hash(state);
        }
    }
}

/// One processed asset: where it lives, how to emit it, and what it depends on.
///
/// This is the value the introspector hands back ([`TwylaIntrospector::value`]),
/// so `asset.*().url()` / `.read()` read straight off it. Its `built.emit` is
/// either in-memory bytes (Arc-shared, cheap to clone) or a path the bytes are
/// read from on demand — so handing the whole record around never pulls asset
/// bytes into RAM.
#[derive(Clone, Debug, PartialEq, Hash)]
pub struct Resolution {
    pub policy: OutputReq,
    pub output_path: String,
    pub url: String,
}

impl Repr for Resolution {
    fn repr(&self) -> EcoString {
        eco_format!("{}", self.url)
    }
}

#[ty]
#[derive(Clone, Debug)]
pub struct ResolvedAsset {
    /// The spec that produced it (its key).
    pub spec: AssetSpec,
    /// Span of a source usage, for diagnostics while resolving output policies.
    pub span: Span,
    /// The built asset
    pub built: Built,
    /// Bundle-relative output path, e.g. `assets/main-<hash>.css`.
    pub output_path: String,
    /// The public, root-relative URL for the auto output (`output_path` resolved
    /// through [`TwylaContext::asset_url`], so it folds in any `base_url`).
    pub url: String,
    /// Per-output-policy resolved paths/URLs for `.url()` usages.
    pub resolutions: Vec<Resolution>,
    /// How this asset is used *this compile* — the union of [`OutputReq`]s over
    /// its call sites, accumulated as requests are discovered. Drives emission
    /// ([`Resolver::emittable_assets`](crate::resolver::Resolver::emittable_assets)):
    /// any non-[`OutputReq::Read`] usage is written as a file. Deliberately
    /// *excluded* from [`Hash`]/[`PartialEq`] below — it's emission policy, not
    /// part of the asset's resolved identity, and the resolved record flows
    /// through introspection convergence, which must not churn as usage grows.
    pub outputs: Vec<OutputReq>,
}

// Identity is the resolved result (spec → built → path → url), *not* `outputs`
// (see the field's doc): a spec resolves to one record regardless of how many
// ways it's referenced, and convergence compares these records by value.
impl PartialEq for ResolvedAsset {
    fn eq(&self, other: &Self) -> bool {
        self.spec == other.spec
            && self.built == other.built
            && self.output_path == other.output_path
            && self.url == other.url
            && self.resolutions == other.resolutions
            && self.outputs == other.outputs
    }
}

impl Hash for ResolvedAsset {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.spec.hash(state);
        self.built.hash(state);
        self.output_path.hash(state);
        self.url.hash(state);
        self.resolutions.hash(state);
        self.outputs.hash(state);
    }
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
    pub fn url_for(&self, policy: &OutputReq) -> &str {
        self.resolutions
            .iter()
            .find(|resolution| &resolution.policy == policy)
            .map(|resolution| resolution.url.as_str())
            .unwrap_or(self.url.as_str())
    }

    /// The on-disk source files this asset depends on — for the serve watcher.
    pub fn upstream_paths(&self) -> impl Iterator<Item = &Path> {
        self.built.upstream.iter().map(|u| u.path.as_path())
    }
}

#[cfg(test)]
mod tests;

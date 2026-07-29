//! `asset.svg` — minify an SVG (via svgm, an svgo port) and optionally inject
//! an `id` attribute on its root `<svg>` element.
//!
//! Like the other asset types it's an *element* ([`SvgAsset`]): calling
//! `asset.svg("icon.svg")` captures the source path and parameters, and
//! `.url()` / `.read()` resolve lazily through the asset loop. `minify` and
//! `id` are settable, so `#set asset.svg(minify: false)` configures a scope.
//!
//! The root `id` exists for `<use href="url#id">`: cross-document `<use>`
//! requires a fragment naming an element, so referencing a whole SVG needs an
//! id on its root node (see MDN's `<use>` usage notes). `id: auto` derives a
//! stable id from the source content hash; `.elem-id()` resolves it so a
//! template can write `<use href="{url}#{id}">` without repeating itself.
//!
//! We drive svgm's stages directly — `parser::parse` → optimization passes →
//! `serializer::serialize` — rather than `optimize_with_config`, so the id is
//! set on the parsed tree between the passes and serialization (which also
//! guarantees no pass can strip it). A source that svgm cannot parse is passed
//! through verbatim with a *warning* (minification is best-effort), unless an
//! `id` was requested — that contract can't be met without parsing, so it's an
//! error.

use super::{
    AssetInput, AssetSource, AssetSpec, Built, OutputReq, Upstream, emit_or_request, resolve_input,
    sha256,
};
use crate::project::TwylaContext;
use crate::render::Emit;
use ecow::{EcoString, EcoVec, eco_format, eco_vec};
use svgm_core::ast::{Attribute, Document, NodeKind};
use typst::diag::{HintedStrResult, SourceDiagnostic, SourceResult};
use typst::foundations::{
    AutoValue, Bytes, NoneValue, Packed, ShowFn, StyleChain, cast, elem, func, scope,
};
use typst::loading::Encoding;
use typst::syntax::Span;

/// Minify an SVG asset and optionally set an `id` on its root element.
///
/// ```typ
/// #context {
///   let icon = asset.svg("icon.svg", id: auto)
///   raw-html("<use href=\"" + icon.url() + "#" + icon.elem-id() + "\"/>")
/// }
/// ```
#[elem(scope, name = "svg")]
pub struct SvgAsset {
    /// A source SVG path (relative to the calling file), or SVG data as bytes.
    #[required]
    pub path: AssetInput,

    /// Minify the output with svgm's default pass set (an svgo port). Part of
    /// the asset key, so minified and verbatim builds of one file are distinct
    /// assets. A source svgm can't parse passes through verbatim with a
    /// warning.
    #[default(true)]
    pub minify: bool,

    /// An `id` attribute for the root `<svg>` element, so `<use href="…#id">`
    /// can reference the whole graphic cross-document. `{none}` (the default)
    /// leaves the root untouched; `{auto}` derives a stable id from the source
    /// content hash; a string is used verbatim. Read the resolved id back with
    /// #link("#elem-id")[`.elem-id()`].
    #[default(SvgId::None)]
    pub id: SvgId,

    /// Bundle-relative output path policy.
    #[default(OutputReq::Auto)]
    pub output: OutputReq,
}

asset_methods!(SvgAsset, spec, output, Some(Encoding::Utf8), extra {
    /// The `id` injected on the root `<svg>` element — for building
    /// `<use href="url#id">` references. Errors when the asset was declared
    /// with `id: none`. Contextual — call it inside `#context`.
    #[func(contextual)]
    fn elem_id(
        engine: &mut ::typst::engine::Engine,
        context: ::comemo::Tracked<::typst::foundations::Context>,
        this: ::typst::foundations::Content,
    ) -> ::typst::diag::HintedStrResult<::typst::foundations::Str> {
        let elem = this.into_packed::<SvgAsset>().unwrap();
        let styles = context.styles()?;
        let spec = spec(&elem, styles)?;
        if matches!(&spec, AssetSpec::Svg { id: SvgId::None, .. }) {
            return Err(::ecow::eco_format!(
                "this svg asset has no root id; declare it with `id: auto` or `id: \"..\"`"
            )
            .into());
        }
        // `Read` marks the asset as needed without forcing emission — whether
        // the file is written is decided by the `.url()`/bare usages.
        match crate::asset::introspect_asset(
            engine,
            spec,
            elem.span(),
            vec![OutputReq::Read],
        ) {
            Some(asset) => Ok(::typst::foundations::Str::from(
                asset.built.elem_id.clone().unwrap_or_default(),
            )),
            None => Ok(::typst::foundations::Str::from(ELEM_ID_PENDING)),
        }
    }
});

/// Placeholder id returned before the asset resolves; like
/// [`ASSET_PENDING`](super::ASSET_PENDING) it never survives convergence.
const ELEM_ID_PENDING: &str = "__twyla-svg-id-pending__";

/// Build an `asset.svg` element's [`AssetSpec`] from its (style-resolved)
/// fields.
fn spec(elem: &Packed<SvgAsset>, styles: StyleChain) -> HintedStrResult<AssetSpec> {
    Ok(AssetSpec::Svg {
        source: resolve_input(&elem.path, elem.span())?,
        minify: elem.minify.get(styles),
        id: elem.id.get_cloned(styles),
    })
}

fn output(elem: &Packed<SvgAsset>, styles: StyleChain) -> HintedStrResult<OutputReq> {
    Ok(elem.output.get_cloned(styles))
}

/// Build an SVG [`AssetSpec`] for the native image rule ([`crate::rules`]): the
/// parameters come entirely from `#set asset.svg(..)` on the style chain, so a
/// markdown `![](icon.svg)` minifies under the same set rules. Mirrors
/// [`image::spec_from_styles`](super::image::spec_from_styles).
pub(crate) fn spec_from_styles(source: AssetSource, styles: StyleChain) -> AssetSpec {
    AssetSpec::Svg {
        source,
        minify: styles.get(SvgAsset::minify),
        id: styles.get_cloned(SvgAsset::id),
    }
}

/// Default show: a bare `asset.svg(..)` emits and renders nothing.
pub const SHOW_RULE: ShowFn<SvgAsset> = |elem, engine, styles| {
    emit_or_request(
        engine,
        spec(elem, styles),
        elem.span(),
        output(elem, styles),
    )
};

/// The `id` field's value: leave the root untouched, derive a stable id from
/// the source content hash, or use a fixed string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SvgId {
    None,
    Auto,
    Fixed(EcoString),
}

cast! {
    SvgId,
    self => match self {
        Self::None => NoneValue.into_value(),
        Self::Auto => AutoValue.into_value(),
        Self::Fixed(v) => v.into_value(),
    },
    _v: NoneValue => Self::None,
    _v: AutoValue => Self::Auto,
    v: EcoString => Self::Fixed(v),
}

/// Minify the source (unless `minify: false`) and inject the requested root
/// `id`. When neither transformation applies the bytes pass through verbatim —
/// `asset.svg(minify: false)` degenerates to a fingerprinted copy.
pub(crate) fn build(
    source: &AssetSource,
    minify: bool,
    id: &SvgId,
    ctx: &TwylaContext,
    span: Span,
    warnings: &mut Vec<SourceDiagnostic>,
) -> SourceResult<Built> {
    // Read the source bytes from disk (tracking the file) or take the inline
    // bytes directly — same split as `image::build`.
    let (upstream, stem, bytes) = match source {
        AssetSource::File(file) => {
            let on_disk = ctx.root.join(file.vpath().get_without_slash());
            let (up, bytes) = Upstream::new_read_bytes(on_disk).map_err(|err| err_at(span, err))?;
            let stem = up
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_owned);
            (vec![up], stem, bytes)
        }
        AssetSource::Bytes(bytes) => (Vec::new(), None, bytes.to_vec()),
    };

    let out = process(&bytes, minify, id, span, warnings)?;
    let elem_id = match id {
        SvgId::None => None,
        SvgId::Fixed(s) => Some(s.to_string()),
        SvgId::Auto => Some(auto_id(&bytes).to_string()),
    };

    let out = Bytes::new(out);
    Ok(Built {
        sha256: sha256(&out),
        emit: Emit::Bytes(out),
        upstream,
        ext: Some("svg".to_owned()),
        stem,
        dimensions: None,
        elem_id,
    })
}

/// Parse → (optionally) minify → inject id → serialize. Returns the output
/// bytes, or the input verbatim when there is nothing to do or the source is
/// unparsable and only best-effort minification was asked for.
fn process(
    bytes: &[u8],
    minify: bool,
    id: &SvgId,
    span: Span,
    warnings: &mut Vec<SourceDiagnostic>,
) -> SourceResult<Vec<u8>> {
    if !minify && matches!(id, SvgId::None) {
        return Ok(bytes.to_vec());
    }

    let text = std::str::from_utf8(bytes)
        .map_err(|err| err_at(span, format_args!("svg source is not valid UTF-8: {err}")))?;

    let mut doc = match svgm_core::parser::parse(text) {
        Ok(doc) => doc,
        // Minification is best-effort, but an id can't be injected into a tree
        // we couldn't parse — passing through silently would break `<use>`.
        Err(err) if matches!(id, SvgId::None) => {
            warnings.push(SourceDiagnostic::warning(
                span,
                eco_format!("could not minify svg: {err}"),
            ));
            return Ok(bytes.to_vec());
        }
        Err(err) => {
            return Err(err_at(
                span,
                format_args!("cannot set an `id` on an svg that failed to parse: {err}"),
            ));
        }
    };

    if minify {
        let passes = svgm_core::config::passes_for_config(&svgm_core::Config::default());
        svgm_core::optimizer::optimize_with_passes(&mut doc, &passes);
    }

    match id {
        SvgId::None => {}
        SvgId::Fixed(value) => set_root_id(&mut doc, value, span)?,
        SvgId::Auto => set_root_id(&mut doc, &auto_id(bytes), span)?,
    }

    Ok(svgm_core::serializer::serialize(&doc).into_bytes())
}

/// Minify an already-generated SVG string (no id handling) — for
/// `asset.typst(format: "svg", minify: true)`. Best-effort: an unparsable
/// input warns and passes through.
pub(crate) fn minify_str(svg: String, span: Span, warnings: &mut Vec<SourceDiagnostic>) -> String {
    match svgm_core::parser::parse(&svg) {
        Ok(mut doc) => {
            let passes = svgm_core::config::passes_for_config(&svgm_core::Config::default());
            svgm_core::optimizer::optimize_with_passes(&mut doc, &passes);
            svgm_core::serializer::serialize(&doc)
        }
        Err(err) => {
            warnings.push(SourceDiagnostic::warning(
                span,
                eco_format!("could not minify svg: {err}"),
            ));
            svg
        }
    }
}

/// The `id: auto` value: derived from the *source* bytes (not the minified
/// output), so it's stable under minifier changes and never feeds back into
/// the fingerprint circularly.
fn auto_id(source: &[u8]) -> EcoString {
    eco_format!("svg-{}", &hex::encode(sha256(source))[..8])
}

/// Set (or replace) the `id` attribute on the document's root `<svg>` element.
fn set_root_id(doc: &mut Document, id: &str, span: Span) -> SourceResult<()> {
    let children: Vec<_> = doc.children(doc.root).collect();
    for child in children {
        if let NodeKind::Element(el) = &mut doc.node_mut(child).kind
            && el.name == "svg"
            && el.prefix.is_none()
        {
            match el
                .attributes
                .iter_mut()
                .find(|a| a.prefix.is_none() && a.name == "id")
            {
                Some(attr) => attr.value = id.to_string(),
                None => el.attributes.push(Attribute {
                    prefix: None,
                    name: "id".to_string(),
                    value: id.to_string(),
                }),
            }
            return Ok(());
        }
    }
    Err(err_at(
        span,
        "svg has no root <svg> element to set an `id` on",
    ))
}

/// A spanned error from any `Display` payload.
fn err_at(span: Span, msg: impl std::fmt::Display) -> EcoVec<SourceDiagnostic> {
    eco_vec![SourceDiagnostic::error(
        span,
        EcoString::from(msg.to_string())
    )]
}

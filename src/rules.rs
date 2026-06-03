//! Native HTML rules — twyla's mechanism 2.
//!
//! `library.rules` is a public [`NativeRuleMap`]; `replace::<E>(Html, fn)`
//! overrides how a typst element serializes to HTML *globally and
//! invisibly*, with none of the ordering quirks of a userland `show`
//! rule. These are behaviors that belong to the engine, not to any one
//! theme — so every site gets them for free and no per-site fixup is
//! needed (see the deleted `show heading` / `show link` rules in
//! `~/Src/site/templates/base.typ`).
//!
//! Each `replace` *panics if no upstream rule exists* for the element, so
//! a future typst rename surfaces loudly here rather than silently
//! reverting to default behavior.
//!
//! Three rules today:
//! - [`HEADING_RULE`] — `= → <h1>` (no offset) plus an auto-slug `id` on
//!   every heading (zola parity). An explicit `<label>` wins, because an
//!   intra-/cross-doc `#link(<slug>)` resolves to `#slug` and the target
//!   heading's `id` must match.
//! - [`LINK_RULE`] — external http(s) links get
//!   `rel="noopener external" target="_blank"`.
//! - [`IMAGE_RULE`] — `<img src>` points at a fingerprinted asset URL
//!   (resolved through [`crate::asset`]) instead of upstream's base64 inline.

use ecow::{EcoString, eco_format};
use typst_html::{HtmlElem, attr, tag};
use typst_library::diag::{At, warning};
use typst_library::foundations::{Derived, NativeElement, NativeRuleMap, Packed, ShowFn, Target};
use typst_library::introspection::Counter;
use typst_library::layout::BlockElem;
use typst_library::loading::DataSource;
use typst_library::model::{Destination, EarlyLinkResolver, HeadingElem, LinkElem};
use typst_library::text::SpaceElem;
use typst_library::visualize::{
    ExchangeFormat, ImageElem, ImageFormat, RasterFormat, VectorFormat,
};

use crate::asset::{AssetSpec, resolve_or_request};

/// Install twyla's native HTML rules into a freshly built library's
/// rule map. Call from [`crate::prelude::install`] after
/// `Library::builder().build()`.
pub fn install(rules: &mut NativeRuleMap) {
    rules.replace(Target::Html, HEADING_RULE);
    rules.replace(Target::Html, LINK_RULE);
    rules.replace(Target::Html, IMAGE_RULE);
}

/// Heading id: an explicit label wins (so `#link(<slug>)` → `#slug`
/// resolves to this heading), otherwise slugify the heading text the way
/// zola auto-anchors do. Empty result → no `id`.
fn heading_id(elem: &Packed<HeadingElem>) -> Option<EcoString> {
    if let Some(label) = elem.label() {
        return Some(label.resolve().as_str().into());
    }
    let slug = slug::slugify(elem.body.plain_text());
    (!slug.is_empty()).then(|| slug.into())
}

/// `= → <h1>` (no level offset — typst-html offsets by one to reserve
/// `<h1>` for the document title, but twyla puts the title in
/// `<head><title>` and treats the first `=` as the page `<h1>`, matching
/// zola). Levels 1–6 map to `<h1>`–`<h6>`; deeper headings fall back to
/// `<div role="heading">` as upstream does. Plus an auto-slug `id` (see
/// [`heading_id`]).
const HEADING_RULE: ShowFn<HeadingElem> = |elem, engine, styles| {
    let span = elem.span();

    let mut realized = elem.body.clone();
    if let Some(numbering) = elem.numbering.get_ref(styles).as_ref() {
        let location = elem.location().unwrap();
        let numbering = Counter::of(HeadingElem::ELEM)
            .display_at(engine, location, styles, numbering, span)?
            .spanned(span);
        realized = numbering + SpaceElem::shared().clone() + realized;
    }

    let id = heading_id(&elem);
    let level = elem.resolve_level(styles).get();
    Ok(BlockElem::packed(if level >= 7 {
        engine.sink.warn(warning!(
            span,
            "heading of level {} was transformed to \
             <div role=\"heading\" aria-level=\"{}\">, which is not \
             supported by all assistive technology",
            level, level;
            hint: "HTML only supports <h1> to <h6>, not <h{}>", level;
            hint: "you may want to restructure your document so that \
                   it doesn't contain deep headings";
        ));
        HtmlElem::new(tag::div)
            .with_body(Some(realized))
            .with_attr(attr::role, "heading")
            .with_attr(attr::aria_level, eco_format!("{}", level))
            .with_optional_attr(attr::id, id)
            .pack()
            .spanned(span)
    } else {
        let t = [tag::h1, tag::h2, tag::h3, tag::h4, tag::h5, tag::h6][level - 1];
        HtmlElem::new(t)
            .with_body(Some(realized))
            .with_optional_attr(attr::id, id)
            .pack()
            .spanned(span)
    }))
};

/// External http(s) links get `rel="noopener external" target="_blank"`
/// (zola's `external_links_target_blank` + `_no_follow`/`_no_referrer`
/// defaults reduce to this on samsartor.com). Internal links (relative
/// URLs, mailto, intra-/cross-doc label destinations) pass through
/// unchanged. Otherwise identical to upstream's `LINK_RULE`.
const LINK_RULE: ShowFn<LinkElem> = |elem, engine, _| {
    let span = elem.span();
    let dest = elem.dest.resolve_early(engine, span)?;

    let mut external = false;
    let href = match dest {
        Destination::Url(url) => {
            let url = url.clone().into_inner();
            external = url.starts_with("http://") || url.starts_with("https://");
            Some(url)
        }
        Destination::Position(_) => {
            engine
                .sink
                .warn(warning!(span, "positional link was ignored during HTML export"));
            None
        }
        Destination::Location(location) => Some(
            EarlyLinkResolver::new(elem.location().unwrap(), span)
                .resolve(engine, location)
                .and_then(|link| link.into_relative_uri())
                .at(span)?,
        ),
    };

    let mut html = HtmlElem::new(tag::a)
        .with_optional_attr(attr::href, href)
        .with_body(Some(elem.body.clone()));
    if external {
        html = html
            .with_attr(attr::rel, "noopener external")
            .with_attr(attr::target, "_blank");
    }
    Ok(html.pack())
};

/// `<img src>` points at a real, fingerprinted asset URL instead of upstream's
/// base64-inline (`WebImage::to_base64_url`) — twyla emits images as files, so
/// the page references them. The src resolves through the asset system's
/// placeholder protocol ([`resolve_or_request`]) off the show rule's live
/// `styles`, so a plain markdown `![](foo.png)` works with no `#context`.
///
/// A path-backed image becomes a verbatim [`AssetSpec::File`] (fingerprinted
/// copy). An inline/byte-source image — no project `FileId` — falls back to a
/// content-addressed [`AssetSpec::Raw`] built from the bytes typst already
/// loaded, with its extension sniffed from those bytes ([`image_ext`]).
const IMAGE_RULE: ShowFn<ImageElem> = |elem, _engine, styles| {
    let span = elem.span();
    let Derived { source, derived: loaded } = &elem.source;

    // A `File` asset needs a project `FileId`; only a resolvable path has one.
    let file = match source {
        DataSource::Path(path) => path.resolve_if_some(span.id()).ok(),
        DataSource::Bytes(_) => None,
    };
    let spec = match file {
        Some(rooted) => AssetSpec::File { file: rooted.intern() },
        None => AssetSpec::Raw {
            bytes: loaded.data.clone(),
            ext: image_ext(&loaded.data),
        },
    };

    let src = resolve_or_request(styles, &spec);

    let mut img = HtmlElem::new(tag::img).with_attr(attr::src, src.as_str());
    if let Some(alt) = elem.alt.get_cloned(styles) {
        img = img.with_attr(attr::alt, alt);
    }

    // TODO: emit width/height for layout reservation. Upstream fully decodes
    // every image just to read intrinsic dimensions for these attrs; we skip
    // that until `AssetSpec::Image` lands and reports dimensions as part of the
    // decode it has to do anyway.
    Ok(BlockElem::packed(img.pack().spanned(span)))
};

/// Sniff an output extension from image bytes (drives the static server's
/// Content-Type) for a byte-source [`AssetSpec::Raw`]. `None` for an
/// unrecognized format → the asset falls back to a `.bin` extension.
fn image_ext(data: &[u8]) -> Option<EcoString> {
    let ext = match ImageFormat::detect(data)? {
        ImageFormat::Raster(RasterFormat::Exchange(ExchangeFormat::Png)) => "png",
        ImageFormat::Raster(RasterFormat::Exchange(ExchangeFormat::Jpg)) => "jpg",
        ImageFormat::Raster(RasterFormat::Exchange(ExchangeFormat::Gif)) => "gif",
        ImageFormat::Raster(RasterFormat::Exchange(ExchangeFormat::Webp)) => "webp",
        ImageFormat::Vector(VectorFormat::Svg) => "svg",
        ImageFormat::Vector(VectorFormat::Pdf) => "pdf",
        _ => return None,
    };
    Some(ext.into())
}

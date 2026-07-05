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
use typst_library::foundations::{
    Derived, Dict, Module, NativeElement, NativeRuleMap, Packed, ShowFn, Target,
};
use typst_library::introspection::Counter;
use typst_library::layout::BlockElem;
use typst_library::loading::DataSource;
use typst_library::model::{Destination, EarlyLinkResolver, HeadingElem, LinkElem};
use typst_library::text::SpaceElem;
use typst_library::visualize::{
    ExchangeFormat, ImageElem, ImageFormat, RasterFormat, VectorFormat,
};

use crate::asset::{
    AssetSpec, ImageSource, OutputReq, image, resolve_image_or_request, resolve_or_request, svg,
};

/// Install twyla's native HTML rules into a freshly built library's
/// rule map. Call from [`crate::prelude::install`] after
/// `Library::builder().build()`.
pub fn install(rules: &mut NativeRuleMap) {
    rules.replace(Target::Html, HEADING_RULE);
    rules.replace(Target::Html, LINK_RULE);
    rules.replace(Target::Html, IMAGE_RULE);

    // An inline `#document(..)[body]` vanishes where it sits (its body is
    // hoisted to its own bundle output). `register` (not `replace`) — twyla owns
    // this element, so there is no upstream rule. Bundle target is intentionally
    // left rule-less so the element survives realization for discovery.
    rules.register(Target::Html, crate::document::RENDER_INTROSPECTION);
    rules.register(Target::Paged, crate::document::RENDER_INTROSPECTION);

    // Assets are usually consumed via `.url()`/`.read()` (which discard the
    // element). A bare `#asset.file(..)` left in markup still requests emission
    // and vanishes, document-style. `register`: twyla owns these elements.
    rules.register(Target::Html, crate::asset::file::SHOW_RULE);
    rules.register(Target::Paged, crate::asset::file::SHOW_RULE);
    rules.register(Target::Html, crate::asset::sass::SHOW_RULE);
    rules.register(Target::Paged, crate::asset::sass::SHOW_RULE);
    rules.register(Target::Html, crate::asset::image::SHOW_RULE);
    rules.register(Target::Paged, crate::asset::image::SHOW_RULE);
    rules.register(Target::Html, crate::asset::svg::SHOW_RULE);
    rules.register(Target::Paged, crate::asset::svg::SHOW_RULE);
    rules.register(Target::Html, crate::asset::typst_doc::SHOW_RULE);
    rules.register(Target::Paged, crate::asset::typst_doc::SHOW_RULE);
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

    // Read base_url from sys.inputs so same-site full URLs are not treated as
    // external (e.g. ref-resolved links like "https://example.com/portfolio/").
    let site_base: EcoString = (|| {
        let sys = engine.library.global.scope().get("sys")?.read().clone();
        let sys = sys.cast::<Module>().ok()?;
        let inputs = sys.field("inputs", ()).ok()?.clone();
        let inputs = inputs.cast::<Dict>().ok()?;
        let bu = inputs.get("base_url").ok()?.clone();
        bu.cast::<EcoString>().ok()
    })()
    .unwrap_or_default();

    let mut external = false;
    let href = match dest {
        Destination::Url(url) => {
            let url = url.clone().into_inner();
            let is_http = url.starts_with("http://") || url.starts_with("https://");
            let is_same_site = !site_base.is_empty() && url.starts_with(site_base.as_str());
            external = is_http && !is_same_site;
            Some(url)
        }
        Destination::Position(_) => {
            engine.sink.warn(warning!(
                span,
                "positional link was ignored during HTML export"
            ));
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
/// placeholder protocol off the show rule's live `styles`, so a plain markdown
/// `![](foo.png)` works with no `#context`.
///
/// **Raster images** (PNG/JPEG/GIF/WebP/…) route through the full `asset.image`
/// pipeline ([`AssetSpec::Image`]), so a markdown `![](photo.png)` behaves like
/// `#show image: it => img(src: asset.image(it.path).url())` — it transcodes /
/// resizes per any `#set asset.image(..)` on the chain ([`image::spec_from_styles`])
/// and emits the output's intrinsic `width`/`height` (from the decode the
/// pipeline does anyway) for layout reservation. A path-backed image becomes an
/// [`ImageSource::File`] (read + watched), an inline/byte-source image an
/// [`ImageSource::Bytes`].
///
/// **SVG images** route through the `asset.svg` pipeline ([`AssetSpec::Svg`]),
/// so a markdown `![](icon.svg)` minifies per any `#set asset.svg(..)` on the
/// chain ([`svg::spec_from_styles`]). **Other vectors** (PDF) stay verbatim: a
/// path-backed one is a fingerprinted [`AssetSpec::File`] copy, a byte-source
/// one a content-addressed [`AssetSpec::Raw`] with its extension sniffed from
/// the bytes ([`image_ext`]).
const IMAGE_RULE: ShowFn<ImageElem> = |elem, engine, styles| {
    let span = elem.span();
    let Derived {
        source,
        derived: loaded,
    } = &elem.source;

    // A project `FileId` (for File-backed assets) needs a resolvable path.
    let file = match source {
        DataSource::Path(path) => path.resolve_if_some(span.id()).ok().map(|r| r.intern()),
        DataSource::Bytes(_) => None,
    };

    // Only raster formats can go through the decode/resize/transcode pipeline;
    // SVGs go through the minify pipeline; other vectors are copied verbatim.
    let detected = ImageFormat::detect(&loaded.data);
    let raster = matches!(detected, Some(ImageFormat::Raster(_)));
    let is_svg = matches!(detected, Some(ImageFormat::Vector(VectorFormat::Svg)));

    let (src, dimensions) = if raster {
        let img_source = match file {
            Some(file) => ImageSource::File(file),
            None => ImageSource::Bytes(loaded.data.clone()),
        };
        let spec = image::spec_from_styles(img_source, styles).at(span)?;
        let resolved = resolve_image_or_request(engine, spec, span);
        (resolved.url, resolved.dimensions)
    } else if is_svg {
        let source = match file {
            Some(file) => ImageSource::File(file),
            None => ImageSource::Bytes(loaded.data.clone()),
        };
        let spec = svg::spec_from_styles(source, styles);
        (
            resolve_or_request(engine, spec, span, OutputReq::Auto),
            None,
        )
    } else {
        let spec = match file {
            Some(file) => AssetSpec::File { file },
            None => AssetSpec::Raw {
                bytes: loaded.data.clone(),
                ext: image_ext(&loaded.data),
            },
        };
        (
            resolve_or_request(engine, spec, span, OutputReq::Auto),
            None,
        )
    };

    let mut img = HtmlElem::new(tag::img).with_attr(attr::src, src.as_str());
    if let Some(alt) = elem.alt.get_cloned(styles) {
        img = img.with_attr(attr::alt, alt);
    }
    // Intrinsic pixel dimensions reserve the image's box before it loads,
    // avoiding layout shift; present once the asset resolves (a miss has none).
    if let Some((w, h)) = dimensions {
        img = img
            .with_attr(attr::width, eco_format!("{w}"))
            .with_attr(attr::height, eco_format!("{h}"));
    }
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

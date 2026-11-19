//! `asset.image` — decode a raster image, optionally resize it, and re-encode
//! it to a chosen format.
//!
//! Like the other asset types it's an *element* ([`ImageAsset`]): calling
//! `asset.image("photo.jpg", width: 600, format: "webp")` captures the source
//! path and the processing parameters, and `.url()` / `.read()` resolve that
//! spec lazily through the asset loop. Every parameter is a settable field, so
//! `#set asset.image(format: "webp", quality: 80)` configures a whole scope —
//! the closest thing to Hugo's site-wide `imaging` defaults until twyla grows a
//! real site-config story.
//!
//! Processing model (a deliberately small slice of Hugo's): **width / height /
//! fit / filter / format / quality**. Drawing-style effects (blur, background
//! fills, rotation, anchors) are intentionally out of scope — those belong in a
//! Typst drawing API, not the asset pipeline.
//!
//! Vector sources (SVG, PDF) are *rasterized* rather than decoded: they need an
//! explicit raster `format` and at least one of `width`/`height`, and are
//! rendered by typst's own image renderers (resvg / hayro, via
//! `typst_render`) directly at the target pixel size — no render-then-resample
//! step, so `filter` doesn't apply. A PDF renders its first page.
//!
//! Encoders: PNG / JPEG / GIF / AVIF go through the `image` crate; lossy WebP
//! goes through libwebp (the `webp` crate), because `image`'s pure-Rust WebP
//! encoder is lossless-only and lossless WebP defeats the point for photos.

use std::io::Cursor;

use super::{
    AssetInput, AssetSource, AssetSpec, Built, OutputReq, emit_or_request, resolve_input, sha256,
    typst_doc,
};
use crate::ledger::DepRecorder;
use crate::project::TwylaContext;
use crate::render::Emit;
use comemo::{Track, Tracked};
use ecow::{EcoString, EcoVec, eco_format, eco_vec};
use image::codecs::avif::AvifEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder, ImageFormat};
use typst::World;
use typst::diag::{HintedStrResult, LoadedWithin, SourceDiagnostic, SourceResult};
use typst::foundations::{
    AutoValue, Bytes, Cast, Content, Packed, ShowFn, Smart, StyleChain, cast, elem, func, scope,
};
use typst::layout::{Abs, Frame, FrameItem, Point, Size};
use typst::loading::{LoadSource, Loaded};
use typst::syntax::{Span, Spanned};
use typst::text::families;
use typst::visualize::{
    Color, Image, ImageFormat as TypstImageFormat, ImageKind, PdfDocument, PdfImage, SvgImage,
    VectorFormat,
};
use typst_kit::packages::SystemPackages;
use typst_layout::Page;

/// Pulls in an image file as an asset.
///
/// ```typ
/// #context html.elem("img", attrs: (
///   src: asset.image("photo.jpg", width: 600, format: "webp").url(),
/// ))
/// ```
///
/// By default, the image file is copied verbatim (same as #link("#file")[file]).
/// If `format` is provided, the image will be transcoded. If `width`/`height` are
/// provided, the image will be resized.
///
/// An SVG or PDF source is rasterized (a PDF's first page), which requires an
/// explicit `format` and at least one of `width`/`height`:
/// `asset.image("diagram.svg", width: 800, format: "png")`.
///
/// All parameters are settable, so `#set asset.image(format: "webp")` applies to
/// any image asset in the scope.
#[elem(scope, name = "image")]
pub struct ImageAsset {
    /// A source image path (relative to the calling file), or image data as
    /// bytes.
    #[required]
    pub path: AssetInput,

    /// Target width in pixels. With only one of `width`/`height` set the other
    /// is computed to preserve the aspect ratio; with neither, the image is not
    /// resized.
    #[default(None)]
    pub width: Option<i64>,

    /// Target height in pixels. See [`width`](Self::width).
    #[default(None)]
    pub height: Option<i64>,

    /// How the image is fit into `width`×`height` when both are given:
    /// `"contain"` scales to fit inside the box (aspect preserved), `"cover"`
    /// scales and crops to fill it (aspect preserved), `"stretch"` forces the
    /// exact dimensions (aspect distorted). `cover`/`stretch` require both
    /// dimensions.
    #[default(Fit::Contain)]
    pub fit: Fit,

    /// The resampling filter used when scaling. `"lanczos"` (the default) gives
    /// the best downscaling quality; cheaper options trade quality for speed.
    /// Ignored for SVG/PDF sources, which render directly at the target size.
    #[default(Filter::Lanczos)]
    pub filter: Filter,

    /// Output format. `{auto}` (the default) preserves the detected source
    /// format; otherwise the image is transcoded to the named format.
    #[default(Format::Auto)]
    pub format: Format,

    /// Encoder quality, 1–100, for lossy formats (JPEG, WebP, AVIF). Ignored by
    /// PNG and GIF.
    #[default(75)]
    pub quality: u8,

    /// Bundle-relative output path policy.
    #[default(OutputReq::Auto)]
    pub output: OutputReq,
}

// Image bytes are binary, so `.read()` defaults to raw bytes (`None`) rather
// than the UTF-8 default the text-ish assets use.
asset_methods!(ImageAsset, spec, output, None);

/// Build an `asset.image` element's [`AssetSpec`] from its (style-resolved)
/// fields. The element is always file-backed; per-call args override the
/// `#set asset.image(..)` defaults the field accessors fold in.
fn spec(elem: &Packed<ImageAsset>, styles: StyleChain) -> HintedStrResult<AssetSpec> {
    let source = resolve_input(&elem.path, elem.span())?;
    image_spec(
        source,
        elem.width.get(styles),
        elem.height.get(styles),
        elem.fit.get(styles),
        elem.filter.get(styles),
        elem.format.get(styles),
        elem.quality.get(styles),
    )
}

fn output(elem: &Packed<ImageAsset>, styles: StyleChain) -> HintedStrResult<OutputReq> {
    Ok(elem.output.get_cloned(styles))
}

/// Build an image [`AssetSpec`] for the native image rule ([`crate::rules`]):
/// the processing parameters come *entirely* from `#set asset.image(..)` on the
/// style chain, so a plain `![](photo.png)` behaves like
/// `asset.image("photo.png")` under the same set rules. `source` is whatever the
/// rule resolved the image to (path- or bytes-backed).
pub(crate) fn spec_from_styles(
    source: AssetSource,
    styles: StyleChain,
) -> HintedStrResult<AssetSpec> {
    image_spec(
        source,
        styles.get(ImageAsset::width),
        styles.get(ImageAsset::height),
        styles.get(ImageAsset::fit),
        styles.get(ImageAsset::filter),
        styles.get(ImageAsset::format),
        styles.get(ImageAsset::quality),
    )
}

/// Assemble an [`AssetSpec::Image`] from resolved parameters, validating the
/// dimensions. Shared by [`spec`] (element) and [`spec_from_styles`] (native
/// rule) so the two stay in lock-step.
#[allow(clippy::too_many_arguments)]
fn image_spec(
    source: AssetSource,
    width: Option<i64>,
    height: Option<i64>,
    fit: Fit,
    filter: Filter,
    format: Format,
    quality: u8,
) -> HintedStrResult<AssetSpec> {
    Ok(AssetSpec::Image {
        source,
        width: dim(width)?,
        height: dim(height)?,
        fit,
        filter,
        format,
        quality,
    })
}

/// Validate a user-supplied dimension: positive, fits in `u32`. `None` passes
/// through (the dimension is unconstrained).
fn dim(value: Option<i64>) -> HintedStrResult<Option<u32>> {
    match value {
        None => Ok(None),
        Some(v) if v > 0 && v <= u32::MAX as i64 => Ok(Some(v as u32)),
        Some(v) => Err(eco_format!("image dimension must be a positive integer, got {v}").into()),
    }
}

/// Default show: a bare `asset.image(..)` emits and renders nothing.
pub const SHOW_RULE: ShowFn<ImageAsset> = |elem, engine, styles| {
    emit_or_request(
        engine,
        spec(elem, styles),
        elem.span(),
        output(elem, styles),
    )
};

/// How an image is fit into the requested `width`×`height`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Cast)]
pub enum Fit {
    /// Scale to fit *inside* the box, preserving aspect ratio (no crop).
    Contain,
    /// Scale and crop to *fill* the box, preserving aspect ratio.
    Cover,
    /// Scale to the exact dimensions, distorting aspect ratio.
    Stretch,
}

/// Resampling filter used when scaling, mapped onto [`image`]'s [`FilterType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Cast)]
pub enum Filter {
    /// Nearest-neighbor — fastest, blocky.
    Nearest,
    /// Linear (triangle) filter.
    Triangle,
    /// Cubic (Catmull-Rom) filter.
    CatmullRom,
    /// Gaussian filter.
    Gaussian,
    /// Lanczos with window 3 — best downscaling quality.
    Lanczos,
}

impl From<Filter> for FilterType {
    fn from(f: Filter) -> Self {
        match f {
            Filter::Nearest => FilterType::Nearest,
            Filter::Triangle => FilterType::Triangle,
            Filter::CatmullRom => FilterType::CatmullRom,
            Filter::Gaussian => FilterType::Gaussian,
            Filter::Lanczos => FilterType::Lanczos3,
        }
    }
}

/// Output image format selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// Preserve the detected source format.
    Auto,
    /// Transcode to an explicit format.
    Explicit(OutputFormat),
}

cast! {
    Format,
    self => match self {
        Self::Auto => AutoValue.into_value(),
        Self::Explicit(v) => v.into_value(),
    },
    _v: AutoValue => Self::Auto,
    v: OutputFormat => Self::Explicit(v),
}

impl Format {
    fn explicit(self) -> Option<OutputFormat> {
        match self {
            Self::Auto => None,
            Self::Explicit(format) => Some(format),
        }
    }
}

/// An explicitly selected raster output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Cast)]
pub enum OutputFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
    Avif,
}

impl OutputFormat {
    /// The output file extension (also the static server's Content-Type key).
    fn ext(self) -> &'static str {
        match self {
            OutputFormat::Png => "png",
            OutputFormat::Jpeg => "jpg",
            OutputFormat::Gif => "gif",
            OutputFormat::Webp => "webp",
            OutputFormat::Avif => "avif",
        }
    }

    /// Map the `image` crate's detected source format to one of ours, for the
    /// `format: auto` case. Returns `None` for formats
    /// we can't re-encode (e.g. an exotic input) — the caller errors.
    fn from_detected(format: ImageFormat) -> Option<OutputFormat> {
        match format {
            ImageFormat::Png => Some(OutputFormat::Png),
            ImageFormat::Jpeg => Some(OutputFormat::Jpeg),
            ImageFormat::Gif => Some(OutputFormat::Gif),
            ImageFormat::WebP => Some(OutputFormat::Webp),
            ImageFormat::Avif => Some(OutputFormat::Avif),
            _ => None,
        }
    }
}

/// Decode the source image, resize it per `fit`/`filter` (if any dimension is
/// given), and re-encode it to `format` (or the source format) at `quality`.
/// An SVG/PDF source is instead rasterized at the target size ([`rasterize`]).
///
/// A file-backed source is read through the recorder (so it joins the asset's
/// ledger deps for invalidation/watching) — like [`file::build`](super::file::build),
/// the bytes are an asset input, not a typst dependency of the page. A
/// bytes-backed source is an inline image typst already loaded (no deps; the
/// spec is keyed by content). Reports the output's pixel `dimensions` so the
/// native rule can emit `<img width height>`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    world: Tracked<dyn World + '_>,
    source: &AssetSource,
    width: Option<u32>,
    height: Option<u32>,
    fit: Fit,
    filter: Filter,
    format: Format,
    quality: u8,
    ctx: &TwylaContext,
    packages: &SystemPackages,
    span: Span,
    rec: &DepRecorder,
) -> SourceResult<Built> {
    // Read the source bytes from disk (tracking the file) or take the inline
    // bytes directly.
    let (stem, bytes) = match source {
        AssetSource::File(file) => {
            let on_disk = ctx.root.join(file.vpath().get_without_slash());
            let bytes = rec.read(&on_disk).map_err(|err| err_at(span, err))?;
            let stem = on_disk
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_owned);
            (stem, bytes)
        }
        AssetSource::Bytes(bytes) => (None, bytes.to_vec()),
    };

    let (img, out_format) = match image::guess_format(&bytes) {
        // Raster: detect the source format up front — it drives both decoding
        // and the "keep source format" default.
        Ok(detected) => {
            let out_format = match format.explicit() {
                Some(f) => f,
                None => OutputFormat::from_detected(detected).ok_or_else(|| {
                    err_at(
                        span,
                        format_args!(
                            "cannot re-encode {detected:?} images; set an explicit `format`"
                        ),
                    )
                })?,
            };
            let img = image::load_from_memory_with_format(&bytes, detected)
                .map_err(|err| err_at(span, format_args!("failed to decode image: {err}")))?;
            (
                resize(img, width, height, fit, filter.into(), span)?,
                out_format,
            )
        }
        Err(err) => match TypstImageFormat::detect(&bytes) {
            Some(TypstImageFormat::Vector(vector)) => {
                let name = vector_name(vector);
                let out_format = format.explicit().ok_or_else(|| {
                    err_at(
                        span,
                        format_args!(
                            "{name} images must be rasterized; set an explicit raster `format`"
                        ),
                    )
                })?;
                if width.is_none() && height.is_none() {
                    return Err(err_at(
                        span,
                        format_args!("rasterizing {name} images needs `width` and/or `height`"),
                    ));
                }
                let opaque = out_format == OutputFormat::Jpeg;
                let vector = Vector {
                    format: vector,
                    source,
                    bytes,
                };
                let img = rasterize(
                    vector, width, height, fit, opaque, world, ctx, packages, span, rec,
                )?;
                (img, out_format)
            }
            _ => return Err(err_at(span, format_args!("not a recognized image: {err}"))),
        },
    };
    let dimensions = Some((img.width(), img.height()));
    let encoded = encode(&img, out_format, quality).map_err(|err| err_at(span, err))?;

    let bytes = Bytes::new(encoded);
    Ok(Built {
        sha256: sha256(&bytes),
        emit: Emit::Bytes(bytes),
        ext: Some(out_format.ext().to_owned()),
        stem,
        dimensions,
        elem_id: None,
        output_path: None,
        companions: Vec::new(),
        preload: Vec::new(),
    })
}

/// Resize per the fit mode, or return the image untouched when no dimension is
/// requested. `cover`/`stretch` need both dimensions; `contain` accepts one
/// (the other is left unconstrained, so the set dimension binds).
fn resize(
    img: DynamicImage,
    width: Option<u32>,
    height: Option<u32>,
    fit: Fit,
    filter: FilterType,
    span: Span,
) -> SourceResult<DynamicImage> {
    match (width, height) {
        (None, None) => Ok(img),
        _ => match fit {
            // `resize` fits *within* (w, h) preserving aspect; an unset side
            // becomes `u32::MAX` so the set side is the binding constraint.
            Fit::Contain => Ok(img.resize(
                width.unwrap_or(u32::MAX),
                height.unwrap_or(u32::MAX),
                filter,
            )),
            Fit::Cover => {
                let (w, h) = both(width, height, "cover", span)?;
                Ok(img.resize_to_fill(w, h, filter))
            }
            Fit::Stretch => {
                let (w, h) = both(width, height, "stretch", span)?;
                Ok(img.resize_exact(w, h, filter))
            }
        },
    }
}

/// The user-facing name of a vector source format, for diagnostics.
fn vector_name(format: VectorFormat) -> &'static str {
    match format {
        VectorFormat::Svg => "SVG",
        VectorFormat::Pdf => "PDF",
    }
}

/// A vector image source to rasterize: its detected format, where it came from
/// (for resolving an SVG's relative `href`s and placing load errors), and its
/// bytes.
struct Vector<'a> {
    format: VectorFormat,
    source: &'a AssetSource,
    bytes: Vec<u8>,
}

/// Rasterize an SVG or PDF (first page) with typst's image renderers, directly
/// at the target pixel size: the image is placed into a frame of the output
/// size (1pt = 1px) per `fit` and rendered with [`typst_render::render`], which
/// builds the vector texture at exactly the painted size. The background is
/// transparent unless `opaque` (JPEG, which has no alpha) asks for white.
///
/// An SVG loads its fonts from the live `world` (using the default text font
/// families as the fallback list) and its linked images through a recording
/// world ([`typst_doc::file_world`]), so they join the asset's deps.
#[allow(clippy::too_many_arguments)]
fn rasterize(
    vector: Vector,
    width: Option<u32>,
    height: Option<u32>,
    fit: Fit,
    opaque: bool,
    world: Tracked<dyn World + '_>,
    ctx: &TwylaContext,
    packages: &SystemPackages,
    span: Span,
    rec: &DepRecorder,
) -> SourceResult<DynamicImage> {
    let data = Bytes::new(vector.bytes);
    let kind = match vector.format {
        VectorFormat::Svg => {
            // Relative hrefs resolve against the SVG file, or (inline bytes)
            // the calling file — as typst's own `image` does.
            let (file, load_source) = match vector.source {
                AssetSource::File(file) => (Some(*file), LoadSource::Path(*file)),
                AssetSource::Bytes(_) => (span.id(), LoadSource::Bytes),
            };
            let loaded = Loaded::new(Spanned::new(load_source, span), data.clone());
            let library = world.library();
            let families: Vec<&str> = families(StyleChain::new(&library.styles))
                .map(|family| family.as_str())
                .collect();
            let files = typst_doc::file_world(world, ctx, packages, rec);
            let files: &dyn World = &files;
            let svg = SvgImage::with_fonts_images(data, files.track(), &families, file)
                .within(&loaded)?;
            ImageKind::Svg(svg)
        }
        VectorFormat::Pdf => {
            let document = PdfDocument::new(data).map_err(|err| {
                err_at(span, format_args!("the PDF could not be loaded ({err:?})"))
            })?;
            let page =
                PdfImage::new(document, 0).ok_or_else(|| err_at(span, "the PDF has no pages"))?;
            ImageKind::Pdf(page)
        }
    };
    let image = Image::plain(kind);

    // Canvas size and the image's placed size, in pixels.
    let aspect = image.width() / image.height();
    let (canvas, placed) = match fit {
        Fit::Contain => {
            let (w, h) = match (width, height) {
                (Some(w), None) => (w as f64, w as f64 / aspect),
                (None, Some(h)) => (h as f64 * aspect, h as f64),
                (Some(w), Some(h)) if w as f64 / h as f64 > aspect => (h as f64 * aspect, h as f64),
                (Some(w), _) => (w as f64, w as f64 / aspect),
                (None, None) => unreachable!("checked by the caller"),
            };
            let size = (w.round().max(1.0), h.round().max(1.0));
            (size, size)
        }
        Fit::Cover => {
            let (w, h) = both(width, height, "cover", span)?;
            let (w, h) = (w as f64, h as f64);
            let placed = if w / h > aspect {
                (w, w / aspect)
            } else {
                (h * aspect, h)
            };
            ((w, h), placed)
        }
        Fit::Stretch => {
            let (w, h) = both(width, height, "stretch", span)?;
            let size = (w as f64, h as f64);
            (size, size)
        }
    };

    // Center the image (only `cover` overflows; the canvas crops it).
    let mut frame = Frame::hard(Size::new(Abs::pt(canvas.0), Abs::pt(canvas.1)));
    let offset = Point::new(
        Abs::pt((canvas.0 - placed.0) / 2.0),
        Abs::pt((canvas.1 - placed.1) / 2.0),
    );
    let placed = Size::new(Abs::pt(placed.0), Abs::pt(placed.1));
    frame.push(offset, FrameItem::Image(image, placed, span));
    let page = Page {
        frame,
        fill: Smart::Custom(opaque.then(|| Color::WHITE.into())),
        numbering: None,
        supplement: Content::empty(),
        number: 1,
    };
    let pixmap = typst_render::render(&page, 1.0);

    let mut rgba = Vec::with_capacity(pixmap.data().len());
    for pixel in pixmap.pixels() {
        let c = pixel.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    let buf = image::RgbaImage::from_raw(pixmap.width(), pixmap.height(), rgba)
        .expect("pixmap is width × height RGBA");
    Ok(DynamicImage::ImageRgba8(buf))
}

/// Require both dimensions for a fit mode that crops/stretches.
fn both(
    width: Option<u32>,
    height: Option<u32>,
    mode: &str,
    span: Span,
) -> SourceResult<(u32, u32)> {
    match (width, height) {
        (Some(w), Some(h)) => Ok((w, h)),
        _ => Err(err_at(
            span,
            format_args!("`fit: \"{mode}\"` needs both `width` and `height`"),
        )),
    }
}

/// Encode a decoded image to the target format. PNG/JPEG/GIF/AVIF go through the
/// `image` crate; WebP routes through libwebp for lossy output (see the module
/// docs).
fn encode(img: &DynamicImage, format: OutputFormat, quality: u8) -> Result<Vec<u8>, EcoString> {
    let mut buf = Vec::new();
    match format {
        OutputFormat::Png => img
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .map_err(|e| eco_format!("PNG encode failed: {e}"))?,
        OutputFormat::Gif => img
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Gif)
            .map_err(|e| eco_format!("GIF encode failed: {e}"))?,
        OutputFormat::Jpeg => JpegEncoder::new_with_quality(&mut buf, quality)
            .encode_image(img)
            .map_err(|e| eco_format!("JPEG encode failed: {e}"))?,
        OutputFormat::Avif => AvifEncoder::new_with_speed_quality(&mut buf, 4, quality)
            .write_image(
                img.as_bytes(),
                img.width(),
                img.height(),
                img.color().into(),
            )
            .map_err(|e| eco_format!("AVIF encode failed: {e}"))?,
        // `image`'s WebP encoder is lossless-only; libwebp does lossy at the
        // requested quality. `from_image` handles the RGB/RGBA conversion.
        OutputFormat::Webp => {
            let encoder = webp::Encoder::from_image(img)
                .map_err(|e| eco_format!("WebP encode failed: {e}"))?;
            buf = encoder.encode(quality as f32).to_vec();
        }
    }
    Ok(buf)
}

/// A spanned image error from any `Display` payload.
fn err_at(span: Span, msg: impl std::fmt::Display) -> EcoVec<SourceDiagnostic> {
    eco_vec![SourceDiagnostic::error(
        span,
        EcoString::from(msg.to_string())
    )]
}

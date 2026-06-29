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
//! Encoders: PNG / JPEG / GIF / AVIF go through the `image` crate; lossy WebP
//! goes through libwebp (the `webp` crate), because `image`'s pure-Rust WebP
//! encoder is lossless-only and lossless WebP defeats the point for photos.

use std::io::Cursor;

use super::{
    AssetSpec, Built, ImageSource, OutputReq, Upstream, emit_or_request, resolve_path, sha256,
};
use crate::project::TwylaContext;
use crate::render::Emit;
use ecow::{EcoString, EcoVec, eco_format, eco_vec};
use image::codecs::avif::AvifEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder, ImageFormat};
use typst::diag::{HintedStrResult, SourceDiagnostic, SourceResult};
use typst::foundations::{Bytes, Cast, Packed, PathOrStr, ShowFn, StyleChain, elem, func, scope};
use typst::syntax::Span;

/// Pulls in an image file as an asset.
///
/// ```typ
/// #context html.elem("img", attrs: (
///   src: asset.image("photo.jpg", width: 600, format: "webp").url(),
/// ))
/// ```
///
/// By default, the image file is copied verbatim (same as #link[file]("#file")).
/// If `format` is provided, the image will be transcoded. If `width`/`height` are
/// provided, the image will be resized.
///
/// All parameters are settable, so `#set asset.image(format: "webp")` applies to
/// any image asset in the scope.
#[elem(scope, name = "image")]
pub struct ImageAsset {
    /// Path to the source image, relative to the calling file.
    #[required]
    pub path: PathOrStr,

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
    #[default(Filter::Lanczos)]
    pub filter: Filter,

    /// Output format. `{none}` (the default) keeps the source format; otherwise
    /// the image is transcoded to the named format.
    #[default(None)]
    pub format: Option<Format>,

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
    let source = ImageSource::File(resolve_path(&elem.path, elem.span())?);
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
    source: ImageSource,
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
    source: ImageSource,
    width: Option<i64>,
    height: Option<i64>,
    fit: Fit,
    filter: Filter,
    format: Option<Format>,
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

/// Output image format. `None` on the spec means "keep the source format".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Cast)]
pub enum Format {
    Png,
    Jpeg,
    Gif,
    Webp,
    Avif,
}

impl Format {
    /// The output file extension (also the static server's Content-Type key).
    fn ext(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpg",
            Format::Gif => "gif",
            Format::Webp => "webp",
            Format::Avif => "avif",
        }
    }

    /// Map the `image` crate's detected source format to one of ours, for the
    /// "keep source format" (`format: none`) case. Returns `None` for formats
    /// we can't re-encode (e.g. an exotic input) — the caller errors.
    fn from_detected(format: ImageFormat) -> Option<Format> {
        match format {
            ImageFormat::Png => Some(Format::Png),
            ImageFormat::Jpeg => Some(Format::Jpeg),
            ImageFormat::Gif => Some(Format::Gif),
            ImageFormat::WebP => Some(Format::Webp),
            ImageFormat::Avif => Some(Format::Avif),
            _ => None,
        }
    }
}

/// Decode the source image, resize it per `fit`/`filter` (if any dimension is
/// given), and re-encode it to `format` (or the source format) at `quality`.
///
/// A file-backed source is read from disk (so it joins the asset's `upstream`
/// set for invalidation/watching) — like [`file::build`](super::file::build),
/// the bytes are an asset input, not a typst dependency of the page. A
/// bytes-backed source is an inline image typst already loaded (no `upstream`;
/// the spec is keyed by content). Reports the output's pixel `dimensions` so the
/// native rule can emit `<img width height>`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    source: &ImageSource,
    width: Option<u32>,
    height: Option<u32>,
    fit: Fit,
    filter: Filter,
    format: Option<Format>,
    quality: u8,
    ctx: &TwylaContext,
    span: Span,
) -> SourceResult<Built> {
    // Read the source bytes from disk (tracking the file) or take the inline
    // bytes directly.
    let (upstream, stem, bytes) = match source {
        ImageSource::File(file) => {
            let on_disk = ctx.root.join(file.vpath().get_without_slash());
            let (up, bytes) = Upstream::new_read_bytes(on_disk).map_err(|err| err_at(span, err))?;
            let stem = up
                .path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_owned);
            (vec![up], stem, bytes)
        }
        ImageSource::Bytes(bytes) => (Vec::new(), None, bytes.to_vec()),
    };

    // Detect the source format up front: it drives both decoding and the
    // "keep source format" default.
    let detected = image::guess_format(&bytes)
        .map_err(|err| err_at(span, format_args!("not a recognized image: {err}")))?;
    let out_format = match format {
        Some(f) => f,
        None => Format::from_detected(detected).ok_or_else(|| {
            err_at(
                span,
                format_args!("cannot re-encode {detected:?} images; set an explicit `format`"),
            )
        })?,
    };

    let img = image::load_from_memory_with_format(&bytes, detected)
        .map_err(|err| err_at(span, format_args!("failed to decode image: {err}")))?;
    let img = resize(img, width, height, fit, filter.into(), span)?;
    let dimensions = Some((img.width(), img.height()));
    let encoded = encode(&img, out_format, quality).map_err(|err| err_at(span, err))?;

    let bytes = Bytes::new(encoded);
    Ok(Built {
        sha256: sha256(&bytes),
        emit: Emit::Bytes(bytes),
        upstream,
        ext: Some(out_format.ext().to_owned()),
        stem,
        dimensions,
        elem_id: None,
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
fn encode(img: &DynamicImage, format: Format, quality: u8) -> Result<Vec<u8>, EcoString> {
    let mut buf = Vec::new();
    match format {
        Format::Png => img
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .map_err(|e| eco_format!("PNG encode failed: {e}"))?,
        Format::Gif => img
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Gif)
            .map_err(|e| eco_format!("GIF encode failed: {e}"))?,
        Format::Jpeg => JpegEncoder::new_with_quality(&mut buf, quality)
            .encode_image(img)
            .map_err(|e| eco_format!("JPEG encode failed: {e}"))?,
        Format::Avif => AvifEncoder::new_with_speed_quality(&mut buf, 4, quality)
            .write_image(
                img.as_bytes(),
                img.width(),
                img.height(),
                img.color().into(),
            )
            .map_err(|e| eco_format!("AVIF encode failed: {e}"))?,
        // `image`'s WebP encoder is lossless-only; libwebp does lossy at the
        // requested quality. `from_image` handles the RGB/RGBA conversion.
        Format::Webp => {
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

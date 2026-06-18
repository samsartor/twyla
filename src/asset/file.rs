//! `asset.file` — reference a project file as an asset, copied verbatim and
//! fingerprinted by its content hash.
//!
//! `asset.file` is an *element* ([`FileAsset`]): calling it captures the path,
//! and `.url()` / `.read()` (its scope methods) resolve that path lazily and
//! drive it through the asset system. Showing a bare `FileAsset` is an error
//! ([`super::show_unresolved`]) — an asset is meant to be consumed, not
//! rendered.

use comemo::Tracked;
use ecow::{EcoString, eco_vec};
use typst::World;
use typst::diag::{HintedStrResult, SourceDiagnostic, SourceResult};
use typst::foundations::{Packed, PathOrStr, ShowFn, StyleChain, elem, func, scope};
use typst::loading::Encoding;
use typst::syntax::{FileId, Span};
use typst_utils::hash128;

use super::{AssetSpec, Built, Upstream, resolve_path, show_unresolved};
use crate::project::TwylaContext;
use crate::render::Emit;

/// Reference a project file as an asset, copied verbatim and fingerprinted.
///
/// ```typ
/// #context html.elem("img", attrs: (src: asset.file("logo.svg").url()))
/// ```
#[elem(scope, name = "file")]
pub struct FileAsset {
    /// Path to the file, relative to the calling file.
    #[required]
    pub path: PathOrStr,
}

asset_methods!(FileAsset, spec, Some(Encoding::Utf8));

/// Build a `asset.file` element's [`AssetSpec`]. A file asset is a verbatim copy
/// keyed only by its path, so the style chain is unused (`_styles`); it takes
/// one solely to share the [`asset_methods`] shape with the other elements.
fn spec(elem: &Packed<FileAsset>, _styles: StyleChain) -> HintedStrResult<AssetSpec> {
    Ok(AssetSpec::File {
        file: resolve_path(&elem.path, elem.span())?,
    })
}

/// Default show: a bare `asset.file(..)` cannot be rendered — resolve it with
/// `.url()`/`.read()`. Registered for the in-page targets in [`crate::rules`].
pub const SHOW_RULE: ShowFn<FileAsset> =
    |elem, _engine, _styles| show_unresolved(elem.span(), "file");

pub(crate) fn build(
    _world: Tracked<dyn World + '_>,
    file: FileId,
    ctx: &TwylaContext,
    span: Span,
) -> SourceResult<Built> {
    let on_disk = ctx.root.join(file.vpath().get_without_slash());
    let (on_disk, bytes) = Upstream::new_read_bytes(on_disk).map_err(|err| {
        eco_vec![SourceDiagnostic::error(
            span,
            EcoString::from(err.to_string())
        )]
    })?;

    Ok(Built {
        content_hash: hash128(&bytes),
        emit: Emit::Copy(on_disk.path.clone()),
        stem: on_disk
            .path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_owned),
        ext: on_disk
            .path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase()),
        upstream: vec![on_disk],
        dimensions: None,
    })
}

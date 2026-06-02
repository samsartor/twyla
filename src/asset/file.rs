//! `asset.file` — reference a project file as an asset, copied verbatim and
//! fingerprinted by its content hash.

use std::path::Path;

use comemo::Tracked;
use ecow::{EcoString, eco_vec};
use typst::World;
use typst::diag::{SourceDiagnostic, SourceResult};
use typst::foundations::{PathOrStr, func};
use typst::syntax::{FileId, Span, Spanned};
use typst_utils::hash128;

use super::{Asset, AssetSpec, Built, Emit, resolve_path};
use crate::project::TwylaContext;

/// Reference a project file as an asset, copied verbatim and fingerprinted.
#[func]
pub fn file(
    /// Path to the file, relative to the calling file.
    path: Spanned<PathOrStr>,
) -> SourceResult<Asset> {
    Ok(Asset::new(AssetSpec::File {
        file: resolve_path(path)?,
    }))
}

/// Read the file, fingerprint it, and emit a verbatim stream-copy. The source
/// is its own (only) upstream.
pub(crate) fn build(
    world: Tracked<dyn World + '_>,
    file: FileId,
    ctx: &TwylaContext,
) -> SourceResult<Built> {
    let bytes = world.file(file).map_err(|err| {
        eco_vec![SourceDiagnostic::error(Span::detached(), EcoString::from(err))]
    })?;
    let vpath = file.vpath().get_without_slash();
    let on_disk = ctx.root.join(vpath);
    let out_ext = Path::new(vpath)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    Ok(Built {
        content_hash: hash128(&bytes),
        emit: Emit::Copy(on_disk.clone()),
        upstream: vec![on_disk],
        out_ext,
    })
}

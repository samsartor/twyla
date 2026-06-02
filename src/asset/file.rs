//! `asset.file` — reference a project file as an asset, copied verbatim and
//! fingerprinted by its content hash.

use comemo::Tracked;
use ecow::{EcoString, eco_vec};
use typst::World;
use typst::diag::{SourceDiagnostic, SourceResult};
use typst::foundations::{PathOrStr, func};
use typst::syntax::{FileId, Span, Spanned};
use typst_utils::hash128;

use super::{Asset, AssetSpec, Built, Emit, Upstream, resolve_path};
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
    _world: Tracked<dyn World + '_>,
    file: FileId,
    ctx: &TwylaContext,
) -> SourceResult<Built> {
    let on_disk = ctx.root.join(file.vpath().get_without_slash());
    let (on_disk, bytes) = Upstream::new_read_bytes(on_disk).map_err(|err| {
        eco_vec![SourceDiagnostic::error(
            Span::detached(),
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
    })
}

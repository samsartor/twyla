//! `asset.file` — reference a project file as an asset, copied verbatim and
//! fingerprinted by its content hash.
//!
//! `asset.file` is an *element* ([`FileAsset`]): calling it captures the path,
//! and `.url()` / `.read()` (its scope methods) resolve that path lazily and
//! drive it through the asset system. Showing a bare `FileAsset` emits the asset
//! and renders nothing.

use super::{AssetSpec, Built, OutputReq, emit_or_request, resolve_path};
use crate::ledger::{DepRecorder, annotate};
use crate::project::TwylaContext;
use crate::render::Emit;
use ecow::{EcoString, eco_vec};
use typst::diag::{HintedStrResult, SourceDiagnostic, SourceResult};
use typst::foundations::{Packed, PathOrStr, ShowFn, StyleChain, elem, func, scope};
use typst::loading::Encoding;
use typst::syntax::{FileId, Span};

/// Pulls in a file verbatim as an asset.
///
/// ```typ
/// #context html.elem("img", attrs: (src: asset.file("logo.svg").url()))
/// ```
#[elem(scope, name = "file")]
pub struct FileAsset {
    /// Path to the file, relative to the calling file.
    #[required]
    pub path: PathOrStr,

    /// Bundle-relative output path policy.
    #[default(OutputReq::Auto)]
    pub output: OutputReq,
}

asset_methods!(FileAsset, spec, output, Some(Encoding::Utf8));

/// Build a `asset.file` element's [`AssetSpec`]. A file asset is a verbatim copy
/// keyed only by its path, so the style chain is unused (`_styles`); it takes
/// one solely to share the [`asset_methods`] shape with the other elements.
fn spec(elem: &Packed<FileAsset>, _styles: StyleChain) -> HintedStrResult<AssetSpec> {
    Ok(AssetSpec::File {
        file: resolve_path(&elem.path, elem.span())?,
    })
}

fn output(elem: &Packed<FileAsset>, styles: StyleChain) -> HintedStrResult<OutputReq> {
    Ok(elem.output.get_cloned(styles))
}

/// Default show: a bare `asset.file(..)` emits and renders nothing.
pub const SHOW_RULE: ShowFn<FileAsset> = |elem, engine, styles| {
    emit_or_request(
        engine,
        spec(elem, styles),
        elem.span(),
        output(elem, styles),
    )
};

/// Fingerprint the file by streaming it through the recorder (never holding
/// it in memory) and emit it as a copy of the source path.
pub(crate) fn build(
    file: FileId,
    ctx: &TwylaContext,
    span: Span,
    rec: &DepRecorder,
) -> SourceResult<Built> {
    let path = ctx.root.join(file.vpath().get_without_slash());
    let err = |err: std::io::Error| {
        eco_vec![SourceDiagnostic::error(
            span,
            EcoString::from(annotate(&path, err).to_string())
        )]
    };
    let hash = rec.open(&path).map_err(err)?.finish().map_err(err)?;

    Ok(Built {
        sha256: hash,
        stem: path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_owned),
        ext: path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase()),
        emit: Emit::Copy(path),
        dimensions: None,
        elem_id: None,
    })
}

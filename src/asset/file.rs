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
use typst::foundations::{Content, Context, PathOrStr, ShowFn, Str, elem, func, scope};
use typst::loading::{Encoding, Readable};
use typst::syntax::{FileId, Span};
use typst_utils::hash128;

use super::{
    AssetSpec, Built, Upstream, read_or_request, resolve_or_request, resolve_path, show_unresolved,
};
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

#[scope]
impl FileAsset {
    /// The resolved, fingerprinted URL of this asset (e.g.
    /// `/assets/logo-<hash>.svg`). Contextual — call it inside `#context`.
    ///
    /// `context` must precede the `this` self-positional: the `#[func]` macro
    /// classifies special params by name and forwards them ahead of ordinary
    /// positionals, and the method call prepends the element as that positional.
    #[func(contextual)]
    fn url(context: Tracked<Context>, this: Content) -> HintedStrResult<Str> {
        let elem = this.into_packed::<FileAsset>().unwrap();
        let spec = AssetSpec::File {
            file: resolve_path(&elem.path, elem.span())?,
        };
        Ok(resolve_or_request(context.styles()?, &spec, elem.span()))
    }

    /// The file's contents — for inlining instead of linking. Mirrors the
    /// native `read` function: UTF-8 `str` by default, raw `bytes` with
    /// `encoding: none`. Contextual.
    #[func(contextual)]
    fn read(
        context: Tracked<Context>,
        this: Content,
        /// The encoding to read the asset with. If `{none}`, returns raw bytes;
        /// otherwise the bytes are decoded as UTF-8 into a string.
        #[named]
        #[default(Some(Encoding::Utf8))]
        encoding: Option<Encoding>,
    ) -> HintedStrResult<Readable> {
        let elem = this.into_packed::<FileAsset>().unwrap();
        let spec = AssetSpec::File {
            file: resolve_path(&elem.path, elem.span())?,
        };
        read_or_request(context.styles()?, &spec, elem.span(), encoding)
    }
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

//! `asset.sass` — compile a Sass/SCSS file to fingerprinted CSS via grass.
//!
//! grass reads `@use`/`@import` partials through a custom [`RecordingFs`], so
//! every file the stylesheet pulls in becomes part of the asset's `upstream`
//! set — that's what makes a partial edit invalidate the compiled CSS (and get
//! watched). The entry file is read through the tracked World (so it's also a
//! typst dependency) and added to the set explicitly.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use comemo::Tracked;
use ecow::{EcoString, eco_format, eco_vec};
use typst::World;
use typst::diag::{SourceDiagnostic, SourceResult};
use typst::foundations::{Bytes, PathOrStr, func};
use typst::syntax::{FileId, Span, Spanned};
use typst_utils::hash128;

use super::{Asset, AssetSpec, Built, Emit, resolve_path};
use crate::project::TwylaContext;

/// Compile a Sass/SCSS file to a fingerprinted CSS asset.
#[func]
pub fn sass(
    /// Path to the `.sass`/`.scss` file, relative to the calling file.
    path: Spanned<PathOrStr>,
) -> SourceResult<Asset> {
    Ok(Asset::new(AssetSpec::Sass {
        file: resolve_path(path)?,
    }))
}

/// Read the entry, compile it (recording every imported file), fingerprint the
/// CSS output, and emit it as bytes.
pub(crate) fn build(
    world: Tracked<dyn World + '_>,
    file: FileId,
    ctx: &TwylaContext,
) -> SourceResult<Built> {
    let bytes = world.file(file).map_err(|err| {
        eco_vec![SourceDiagnostic::error(Span::detached(), EcoString::from(err))]
    })?;
    let src = std::str::from_utf8(&bytes).map_err(|e| {
        eco_vec![SourceDiagnostic::error(
            Span::detached(),
            eco_format!("sass source is not valid UTF-8: {e}"),
        )]
    })?;
    let on_disk = ctx.root.join(file.vpath().get_without_slash());

    // `.sass` is the indented syntax; everything else (`.scss`) is SCSS —
    // grass's `from_string` would otherwise assume SCSS for both.
    let syntax = match on_disk.extension().and_then(|e| e.to_str()) {
        Some("sass") => grass::InputSyntax::Sass,
        _ => grass::InputSyntax::Scss,
    };

    let fs = RecordingFs::default();
    let mut options = grass::Options::default().input_syntax(syntax).fs(&fs);
    if let Some(parent) = on_disk.parent() {
        options = options.load_path(parent);
    }
    let css = grass::from_string(src.to_string(), &options).map_err(|e| {
        eco_vec![SourceDiagnostic::error(Span::detached(), eco_format!("sass: {e}"))]
    })?;

    // Upstream = the imports grass read + the entry (read via the World, so not
    // seen by the recording fs).
    let mut upstream = fs.reads();
    upstream.push(on_disk);

    let css = Bytes::new(css.into_bytes());
    Ok(Built {
        content_hash: hash128(&css),
        emit: Emit::Bytes(css),
        upstream,
        out_ext: "css".to_string(),
    })
}

/// A grass [`Fs`](grass::Fs) that delegates to disk and records every file it
/// successfully *reads* (not merely probes), capturing the transitive
/// `@use`/`@import` set. Single-threaded (grass compiles synchronously), so a
/// `RefCell` is fine.
#[derive(Debug, Default)]
struct RecordingFs {
    reads: RefCell<BTreeSet<PathBuf>>,
}

impl RecordingFs {
    /// The files read so far, as a fresh `Vec`.
    fn reads(&self) -> Vec<PathBuf> {
        self.reads.borrow().iter().cloned().collect()
    }
}

impl grass::Fs for RecordingFs {
    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let data = std::fs::read(path)?;
        self.reads.borrow_mut().insert(path.to_path_buf());
        Ok(data)
    }
}

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
use std::path::Path;

use super::{AssetSpec, Built, OutputReq, Upstream, emit_or_request, resolve_path, sha256};
use crate::project::TwylaContext;
use crate::render::Emit;
use comemo::Tracked;
use ecow::{EcoString, eco_format, eco_vec};
use typst::World;
use typst::diag::{HintedStrResult, SourceDiagnostic, SourceResult};
use typst::foundations::{Bytes, Packed, PathOrStr, ShowFn, StyleChain, elem, func, scope};
use typst::loading::Encoding;
use typst::syntax::{FileId, Span};

/// Compile a Sass/SCSS file to a fingerprinted CSS asset.
///
/// `minify` is a settable field, so `#set asset.sass(minify: false)` switches a
/// whole page (or site) to expanded output.
///
/// ```typ
/// #context html.elem("link", attrs: (
///   rel: "stylesheet",
///   href: asset.sass("main.scss").url(),
/// ))
/// ```
#[elem(scope, name = "sass")]
pub struct SassAsset {
    /// Path to the `.sass`/`.scss` file, relative to the calling file.
    #[required]
    pub path: PathOrStr,

    /// Minify the output with grass's compressed style (strips whitespace,
    /// comments, and other redundant characters). Part of the asset key, so the
    /// minified and expanded builds of one file resolve to distinct assets.
    #[default(true)]
    pub minify: bool,

    /// Bundle-relative output path policy.
    #[default(OutputReq::Auto)]
    pub output: OutputReq,
}

asset_methods!(SassAsset, spec, output, Some(Encoding::Utf8));

/// Build a `asset.sass` element's [`AssetSpec`] from its (style-resolved)
/// fields: the source path plus the `minify` flag (so the minified and expanded
/// builds of one file are distinct assets).
fn spec(elem: &Packed<SassAsset>, styles: StyleChain) -> HintedStrResult<AssetSpec> {
    Ok(AssetSpec::Sass {
        file: resolve_path(&elem.path, elem.span())?,
        minify: elem.minify.get(styles),
    })
}

fn output(elem: &Packed<SassAsset>, styles: StyleChain) -> HintedStrResult<OutputReq> {
    Ok(elem.output.get_cloned(styles))
}

/// Default show: a bare `asset.sass(..)` emits and renders nothing.
pub const SHOW_RULE: ShowFn<SassAsset> = |elem, engine, styles| {
    emit_or_request(
        engine,
        spec(elem, styles),
        elem.span(),
        output(elem, styles),
    )
};

pub(crate) fn build(
    _world: Tracked<dyn World + '_>,
    file: FileId,
    minify: bool,
    ctx: &TwylaContext,
    span: Span,
) -> SourceResult<Built> {
    let on_disk = ctx.root.join(file.vpath().get_without_slash());
    let (on_disk, src) = Upstream::new_read_string(on_disk).map_err(|err| {
        eco_vec![SourceDiagnostic::error(
            span,
            EcoString::from(err.to_string())
        )]
    })?;
    let stem = on_disk
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_owned);

    // `.sass` is the indented syntax; everything else (`.scss`) is SCSS —
    // grass's `from_string` would otherwise assume SCSS for both.
    let syntax = match on_disk.path.extension().and_then(|e| e.to_str()) {
        Some("sass") => grass::InputSyntax::Sass,
        _ => grass::InputSyntax::Scss,
    };

    let style = if minify {
        grass::OutputStyle::Compressed
    } else {
        grass::OutputStyle::Expanded
    };

    let fs = RecordingFs::default();
    let mut options = grass::Options::default()
        .input_syntax(syntax)
        .style(style)
        .fs(&fs);
    if let Some(parent) = on_disk.path.parent() {
        options = options.load_path(parent);
    }
    let css = grass::from_string(src.to_string(), &options)
        .map_err(|e| eco_vec![SourceDiagnostic::error(span, eco_format!("sass: {e}"))])?;

    // Upstream = the imports grass read + the entry (read via the World, so not
    // seen by the recording fs).
    let mut upstream = fs.reads();
    upstream.push(on_disk);

    let css = Bytes::new(css.into_bytes());
    Ok(Built {
        sha256: sha256(&css),
        emit: Emit::Bytes(css),
        upstream,
        ext: Some("css".to_string()),
        stem,
        dimensions: None,
        elem_id: None,
    })
}

/// A grass [`Fs`](grass::Fs) that delegates to disk and records every file it
/// successfully *reads* (not merely probes), capturing the transitive
/// `@use`/`@import` set. Single-threaded (grass compiles synchronously), so a
/// `RefCell` is fine.
#[derive(Debug, Default)]
struct RecordingFs {
    reads: RefCell<BTreeSet<Upstream>>,
}

impl RecordingFs {
    /// The files read so far, as a fresh `Vec`.
    fn reads(&self) -> Vec<Upstream> {
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
        let (upstream, data) = Upstream::new_read_bytes(path.to_path_buf())?;
        self.reads.borrow_mut().insert(upstream);
        Ok(data)
    }
}

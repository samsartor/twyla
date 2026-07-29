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

use super::{
    AssetInput, AssetSource, AssetSpec, Built, OutputReq, Upstream, emit_or_request, resolve_input,
    sha256,
};
use crate::project::TwylaContext;
use crate::render::Emit;
use comemo::Tracked;
use ecow::{EcoString, eco_format, eco_vec};
use typst::World;
use typst::diag::{HintedStrResult, SourceDiagnostic, SourceResult};
use typst::foundations::{
    AutoValue, Bytes, Cast, Packed, ShowFn, StyleChain, cast, elem, func, scope,
};
use typst::loading::Encoding;
use typst::syntax::Span;

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
    /// A `.sass`/`.scss` path relative to the calling file, or stylesheet
    /// source as bytes.
    #[required]
    pub path: AssetInput,

    /// Input syntax. For file sources, `{auto}` detects Sass from a `.sass`
    /// extension and otherwise uses SCSS. Byte sources have no extension and
    /// therefore require an explicit `"sass"` or `"scss"` format.
    #[default(Format::Auto)]
    pub format: Format,

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
        source: resolve_input(&elem.path, elem.span())?,
        format: elem.format.get(styles),
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
    source: &AssetSource,
    format: Format,
    minify: bool,
    ctx: &TwylaContext,
    span: Span,
) -> SourceResult<Built> {
    let (entry, stem, syntax, src, load_path) = match source {
        AssetSource::File(file) => {
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
            let detected = match on_disk.path.extension().and_then(|e| e.to_str()) {
                Some("sass") => Syntax::Sass,
                _ => Syntax::Scss,
            };
            let syntax = format.explicit().unwrap_or(detected).into();
            let load_path = on_disk.path.parent().map(Path::to_path_buf);
            (Some(on_disk), stem, syntax, src.to_string(), load_path)
        }
        // With no filename there is no extension from which to infer syntax.
        // Imports are rooted at the project root, a stable and unsurprising
        // base for an otherwise location-free source.
        AssetSource::Bytes(bytes) => {
            let syntax: grass::InputSyntax = format
                .explicit()
                .ok_or_else(|| {
                    eco_vec![
                        SourceDiagnostic::error(
                            span,
                            "sass byte sources require an explicit `format`"
                        )
                        .with_hint("set `format: \"scss\"` or `format: \"sass\"`")
                    ]
                })?
                .into();
            let src = std::str::from_utf8(bytes)
                .map_err(|err| {
                    eco_vec![SourceDiagnostic::error(
                        span,
                        eco_format!("sass source is not valid UTF-8: {err}")
                    )]
                })?
                .to_owned();
            (None, None, syntax, src, Some(ctx.root.clone()))
        }
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
    if let Some(parent) = load_path.as_deref() {
        options = options.load_path(parent);
    }
    let css = grass::from_string(src.to_string(), &options)
        .map_err(|e| eco_vec![SourceDiagnostic::error(span, eco_format!("sass: {e}"))])?;

    // Upstream = the imports grass read + a file-backed entry (not seen by the
    // recording fs). Inline bytes are content-addressed by the spec.
    let mut upstream = fs.reads();
    if let Some(entry) = entry {
        upstream.push(entry);
    }

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

/// Sass input syntax selection: infer it from a file extension, or specify it
/// explicitly. Inline bytes cannot use [`Auto`](Self::Auto).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// Infer syntax from the source file's extension.
    Auto,
    /// Use an explicit input syntax.
    Explicit(Syntax),
}

cast! {
    Format,
    self => match self {
        Self::Auto => AutoValue.into_value(),
        Self::Explicit(v) => v.into_value(),
    },
    _v: AutoValue => Self::Auto,
    v: Syntax => Self::Explicit(v),
}

impl Format {
    fn explicit(self) -> Option<Syntax> {
        match self {
            Self::Auto => None,
            Self::Explicit(syntax) => Some(syntax),
        }
    }
}

/// An explicitly selected Sass input syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Cast)]
pub enum Syntax {
    /// Indented Sass syntax (`.sass`).
    Sass,
    /// Brace-and-semicolon SCSS syntax (`.scss`).
    Scss,
}

impl From<Syntax> for grass::InputSyntax {
    fn from(value: Syntax) -> Self {
        match value {
            Syntax::Sass => Self::Sass,
            Syntax::Scss => Self::Scss,
        }
    }
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

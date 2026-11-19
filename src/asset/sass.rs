//! `asset.sass` — compile a Sass/SCSS file to fingerprinted CSS via grass.
//!
//! grass does *all* its file I/O — the entry file, every `@use`/`@import`
//! partial, and every existence probe its import resolution makes — through
//! [`RecordingFs`], which forwards to the resolver's
//! [`DepRecorder`](crate::ledger::DepRecorder). So the asset's ledger deps
//! hold every file the stylesheet pulled in *and* every candidate path it
//! tried (`_partial.scss`, `partial.sass`, `partial/_index.scss`, …): editing a
//! partial invalidates the CSS, and creating a file that an import would now
//! resolve to — or that a broken `@use` was looking for — does too. Recorded
//! whether or not the compile succeeds.

use std::io;
use std::path::Path;

use super::{
    AssetInput, AssetSource, AssetSpec, Built, OutputReq, emit_or_request, resolve_input, sha256,
};
use crate::ledger::DepRecorder;
use crate::project::TwylaContext;
use crate::render::Emit;
use ecow::{eco_format, eco_vec};
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
    source: &AssetSource,
    format: Format,
    minify: bool,
    ctx: &TwylaContext,
    span: Span,
    rec: &DepRecorder,
) -> SourceResult<Built> {
    let style = if minify {
        grass::OutputStyle::Compressed
    } else {
        grass::OutputStyle::Expanded
    };
    let fs = RecordingFs { rec };
    let options = grass::Options::default().style(style).fs(&fs);
    let sass_err =
        |e: Box<grass::Error>| eco_vec![SourceDiagnostic::error(span, eco_format!("sass: {e}"))];

    let (stem, css) = match source {
        // The entry is read by grass itself (`from_path`), through the
        // recording fs like every import. Relative imports resolve against
        // the entry's directory; it's also the load path.
        AssetSource::File(file) => {
            let path = ctx.root.join(file.vpath().get_without_slash());
            let stem = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned);
            let detected = match path.extension().and_then(|e| e.to_str()) {
                Some("sass") => Syntax::Sass,
                _ => Syntax::Scss,
            };
            let mut options = options.input_syntax(format.explicit().unwrap_or(detected).into());
            if let Some(parent) = path.parent() {
                options = options.load_path(parent);
            }
            (stem, grass::from_path(&path, &options).map_err(sass_err)?)
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
            let options = options.input_syntax(syntax).load_path(&ctx.root);
            (None, grass::from_string(src, &options).map_err(sass_err)?)
        }
    };

    let css = Bytes::new(css.into_bytes());
    Ok(Built {
        sha256: sha256(&css),
        emit: Emit::Bytes(css),
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

/// A grass [`Fs`](grass::Fs) that does every access through a
/// [`DepRecorder`]: reads (including failed ones, and the entry file) and the
/// `is_file`/`is_dir` probes of import resolution. `canonicalize` keeps
/// grass's default (the path as-is), so deps are recorded under the paths
/// grass actually touched.
#[derive(Debug)]
struct RecordingFs<'a> {
    rec: &'a DepRecorder,
}

impl grass::Fs for RecordingFs<'_> {
    fn is_dir(&self, path: &Path) -> bool {
        self.rec.is_dir(path)
    }

    fn is_file(&self, path: &Path) -> bool {
        self.rec.is_file(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.rec.read(path)
    }
}

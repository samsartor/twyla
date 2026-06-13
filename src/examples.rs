//! `--test-examples`: compile every code block in the site and fail if any
//! don't compile.
//!
//! After a normal build, [`validate`] queries the realized content for `raw`
//! blocks tagged `typ`/`typst`/`example`, and compiles each one as a throwaway
//! *one-page site* in the context of the site being built (a [`SnippetWorld`]
//! that wraps the live [`RenderWorld`], swapping only `main`). Because file
//! loading delegates to that world, an example can `#import "/templates/.."` or
//! reference an `asset.file(..)` exactly as the page it ships in would — the
//! examples are validated against the real project, not a fixture. The compiled
//! output is discarded; only the pass/fail matters.
//!
//! This naturally covers examples inside reflected doc comments too: once the
//! `--reflect` reference page evals a `///` comment, its ` ```typ ` blocks are
//! realized `raw` elements like any other, so the same harvest picks them up.
//! Opt-in (`--test-examples` / `TWYLA_TEST_EXAMPLES`); normal builds skip it.

use typst::diag::{FileResult, Warned};
use typst::foundations::{Bytes, Content, Datetime, Duration, NativeElement, StyleChain};
use typst::introspection::Introspector;
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook, RawContent, RawElem};
use typst::utils::LazyHash;
use typst::{Library, World};
use typst_bundle::Bundle;
use typst_kit::diagnostics::DiagnosticWorld;

use crate::asset::AssetResolver;
use crate::render::{
    RenderError, RenderErrorKind, RenderWorld, emit_to_string, plain_err, terminal_color,
};

/// Languages whose code blocks are treated as compilable examples.
const EXAMPLE_LANGS: &[&str] = &["example"];

/// One harvested code block: its source and the file it was written in.
struct Example {
    code: String,
    origin: Option<String>,
}

/// Compile every example block in `bundle` (against `world`'s context) and
/// return an error listing the ones that failed. `Ok(())` if all compile.
pub fn validate(world: &RenderWorld, bundle: &Bundle) -> Result<(), RenderError> {
    let examples = harvest(bundle);
    let color = terminal_color();

    let mut failures = String::new();
    let mut failed = 0;
    for ex in &examples {
        if let Err(report) = compile_one(world, &ex.code, color) {
            failed += 1;
            let origin = ex.origin.as_deref().unwrap_or("<unknown>");
            failures.push_str(&format!("\nexample in {origin}:\n{report}"));
        }
    }

    if failed == 0 {
        Ok(())
    } else {
        Err(plain_err(
            format!(
                "{failed} of {} example(s) failed to compile:\n{failures}",
                examples.len()
            ),
            RenderErrorKind::Compile,
        ))
    }
}

/// Collect every `typ`/`example` code block from the realized content.
fn harvest(bundle: &Bundle) -> Vec<Example> {
    bundle
        .introspector
        .query(&RawElem::ELEM.select())
        .into_iter()
        .filter_map(|content: Content| {
            let raw = content.into_packed::<RawElem>().ok()?;
            let lang = raw
                .lang
                .get_ref(StyleChain::default())
                .as_ref()
                .map(|s| s.to_lowercase());
            if !EXAMPLE_LANGS.contains(&lang.as_deref().unwrap_or("")) {
                return None;
            }
            Some(Example {
                code: raw_text(&raw.text),
                origin: raw
                    .span()
                    .id()
                    .map(|id| id.vpath().get_without_slash().to_string()),
            })
        })
        .collect()
}

/// Compile one snippet as a one-page site. On failure, returns the rendered
/// diagnostics (while the snippet world is still alive to resolve spans).
fn compile_one(inner: &RenderWorld, code: &str, color: bool) -> Result<(), String> {
    // Path under `content/` so the throwaway page's output derives cleanly
    // (`default_document_output` requires it); the output itself is discarded.
    let id = FileId::unique(RootedPath::new(
        VirtualRoot::Project,
        VirtualPath::new("content/__twyla_example__.typ").unwrap(),
    ));
    let world = SnippetWorld {
        inner,
        id,
        source: Source::new(id, code.to_string()),
    };

    let mut resolver = AssetResolver::new(&inner.ctx);
    let Warned { output, .. } =
        crate::compile::compile_bundle(&inner.ctx, &world, &[id], &mut resolver);
    match output {
        Ok(_) => Ok(()),
        Err(errors) => Err(emit_to_string(&world, &errors, color)),
    }
}

/// Reconstruct a raw block's source text.
fn raw_text(content: &RawContent) -> String {
    match content {
        RawContent::Text(s) => s.to_string(),
        RawContent::Lines(lines) => lines
            .iter()
            .map(|(line, _)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// A [`World`] serving a single in-memory snippet as `main`, delegating
/// everything else (library, fonts, files, imports) to the live build world —
/// so the snippet compiles in the same project context as the page it's from.
struct SnippetWorld<'a> {
    inner: &'a RenderWorld,
    id: FileId,
    source: Source,
}

impl World for SnippetWorld<'_> {
    fn library(&self) -> &LazyHash<Library> {
        self.inner.library()
    }
    fn book(&self) -> &LazyHash<FontBook> {
        self.inner.book()
    }
    fn main(&self) -> FileId {
        self.id
    }
    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.id {
            Ok(self.source.clone())
        } else {
            self.inner.source(id)
        }
    }
    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if id == self.id {
            Ok(Bytes::new(self.source.text().as_bytes().to_vec()))
        } else {
            self.inner.file(id)
        }
    }
    fn font(&self, index: usize) -> Option<Font> {
        self.inner.font(index)
    }
    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        self.inner.today(offset)
    }
}

impl DiagnosticWorld for SnippetWorld<'_> {
    fn name(&self, id: FileId) -> String {
        if id == self.id {
            "<example>".to_string()
        } else {
            self.inner.name(id)
        }
    }
}

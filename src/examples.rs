//! `twyla-examples`: validate (and highlight) ` ```example ` blocks.
//!
//! Gated like [`crate::reflect`] — installed only under `--test-examples` /
//! `TWYLA_TEST_EXAMPLES`. A docs page applies the example show rule:
//!
//! ```typ
//! show raw.where(lang: "example"): twyla-examples.compile-example
//! ```
//!
//! For each ` ```example ` block, [`compile_example`] compiles the code as a
//! throwaway one-page HTML document and **fails the build if it doesn't
//! compile** (typst's `example` model — validation falls out of rendering the
//! docs). It then re-emits the source tagged `typ` so it syntax-highlights (an
//! `example` lang tag wouldn't). Output rendering (an adjacent iframe of the
//! compiled page) is a planned follow-up.
//!
//! The snippet compiles against a [`SnippetWorld`] that clones the build's
//! library + font book and delegates file loads to the live world, so an
//! example can use twyla builtins and `#import` project files. It does *not*
//! carry twyla's `AssetResolver`, so `asset.file(..)` compiles but its URL
//! won't resolve — examples are validated for typst, not asset existence.

use comemo::Tracked;
use ecow::EcoString;
use typst::diag::{FileResult, SourceResult, Trace, Tracepoint};
use typst::engine::Engine;
use typst::foundations::{Bytes, Content, Datetime, Duration, Module, NativeElement, Packed, Scope, func};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook, RawContent, RawElem};
use typst::utils::LazyHash;
use typst::{Library, World};
use typst_html::HtmlDocument;

/// Install the `twyla-examples` module (exposing [`compile_example`]) into the
/// global scope. Called from [`crate::render::install_stdlib`] only under the
/// `--test-examples` opt-in.
pub fn install(global: &mut Scope) {
    let mut scope = Scope::new();
    scope.define_func::<compile_example>();
    global.define("twyla-examples", Module::new("twyla-examples", scope));
}

/// Compile a `raw` example block as a throwaway one-page site, erroring (and so
/// failing the build) if it doesn't compile, and return the source re-tagged as
/// `typ` for syntax highlighting.
///
/// Wire it up with `show raw.where(lang: "example"): twyla-examples.compile-example`.
#[func]
pub fn compile_example(
    engine: &mut Engine,
    /// The `raw` block to compile and re-display. Passed whole (not just its
    /// text) so diagnostics can point into the example.
    raw: Packed<RawElem>,
) -> SourceResult<Content> {
    let code = raw_text(&raw.text);

    // A throwaway page under `content/` (so output derivation is happy); the
    // compiled document is discarded — only the pass/fail matters. The id is
    // *stable* (not `FileId::unique`) so the nested compile stays deterministic
    // for comemo; distinct examples differ by source content, not id.
    let id = FileId::new(RootedPath::new(
        VirtualRoot::Project,
        VirtualPath::new("content/__twyla_example__.typ").unwrap(),
    ));
    let world = SnippetWorld::new(engine.world, id, Source::new(id, code.clone()));

    let warned = typst::compile::<HtmlDocument>(&world);
    for warning in warned.warnings {
        engine.sink.warn(warning);
    }
    warned
        .output
        .trace(engine.world, || Tracepoint::Call(None), raw.span())?;

    Ok(RawElem::new(RawContent::Text(code.into()))
        .with_lang(Some(EcoString::inline("typ")))
        .with_block(true)
        .pack())
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

/// A [`World`] serving a single in-memory snippet as `main`. Owns a clone of the
/// build's library + font book (those return references, so can't be borrowed
/// across the `Tracked` boundary) and delegates everything else — files,
/// fonts, `today` — to the live world, so the snippet sees the real project.
struct SnippetWorld<'a> {
    world: Tracked<'a, dyn World + 'a>,
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    id: FileId,
    source: Source,
}

impl<'a> SnippetWorld<'a> {
    fn new(world: Tracked<'a, dyn World + 'a>, id: FileId, source: Source) -> Self {
        Self {
            library: world.library().clone(),
            book: world.book().clone(),
            world,
            id,
            source,
        }
    }
}

impl World for SnippetWorld<'_> {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }
    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }
    fn main(&self) -> FileId {
        self.id
    }
    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.id {
            Ok(self.source.clone())
        } else {
            self.world.source(id)
        }
    }
    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if id == self.id {
            Ok(Bytes::new(self.source.text().as_bytes().to_vec()))
        } else {
            self.world.file(id)
        }
    }
    fn font(&self, index: usize) -> Option<Font> {
        self.world.font(index)
    }
    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        self.world.today(offset)
    }
}

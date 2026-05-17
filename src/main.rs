// Throwaway experiment harness for twyla.
//
// Goal: implement the smallest possible `typst::World`, compile a single
// `.typ` file to HTML via typst 0.14's typed-HTML API, and dump:
//   - resolved document info (title, author)
//   - all `metadata(..)` payloads queried back out of the introspector
//   - the rendered HTML string
//   - every World::file() request the compiler made (asset-hook trace)
//
// None of this code is meant to live — it answers feasibility questions
// for the real twyla design.

use std::sync::Mutex;

use comemo::Prehashed;
use typst::diag::{FileError, FileResult, Warned};
use typst::foundations::{Bytes, Datetime, Selector, Smart};
use typst::syntax::{FileId, Source, VirtualPath};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_library::Feature;
use typst_html::HtmlDocument;
use typst_kit::fonts::{FontSlot, Fonts};

struct ExperimentWorld {
    main_id: FileId,
    source: Source,
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<FontSlot>,
    asset_log: Mutex<Vec<String>>,
}

impl ExperimentWorld {
    fn new(typ_source: &str) -> Self {
        let fonts = Fonts::searcher().include_system_fonts(true).search();
        let main_id = FileId::new(None, VirtualPath::new("/main.typ"));
        let source = Source::new(main_id, typ_source.to_string());
        Self {
            main_id,
            source,
            library: LazyHash::new(
                Library::builder()
                    .with_features([Feature::Html].into_iter().collect())
                    .build(),
            ),
            book: LazyHash::new(fonts.book),
            fonts: fonts.fonts,
            asset_log: Mutex::new(Vec::new()),
        }
    }
}

impl World for ExperimentWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }
    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }
    fn main(&self) -> FileId {
        self.main_id
    }
    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main_id {
            Ok(self.source.clone())
        } else {
            Err(FileError::NotFound(
                id.vpath().as_rootless_path().to_path_buf(),
            ))
        }
    }
    fn file(&self, id: FileId) -> FileResult<Bytes> {
        let path = id.vpath().as_rootless_path().to_string_lossy().to_string();
        self.asset_log.lock().unwrap().push(path.clone());
        // Serve a 1x1 transparent SVG for any request so we exercise the
        // happy path of the asset hook without needing real files.
        const STUB_SVG: &[u8] =
            b"<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'></svg>";
        Ok(Bytes::new(STUB_SVG.to_vec()))
    }
    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.get(index)?.get()
    }
    fn today(&self, _offset: Option<i64>) -> Option<Datetime> {
        Datetime::from_ymd(2026, 5, 17)
    }
}

fn main() {
    let typ_src = include_str!("../experiment.typ");
    let world = ExperimentWorld::new(typ_src);

    let Warned { output, warnings } = typst::compile::<HtmlDocument>(&world);

    println!("=== Warnings ({}) ===", warnings.len());
    for w in &warnings {
        println!("  {}", w.message);
    }

    let doc = match output {
        Ok(d) => d,
        Err(errors) => {
            println!("\n=== Errors ({}) ===", errors.len());
            for e in &errors {
                println!("  {}: {}", e.span.id().map(|i| format!("{:?}", i)).unwrap_or_default(), e.message);
            }
            println!("\n=== Asset requests ({}) ===", world.asset_log.lock().unwrap().len());
            for p in world.asset_log.lock().unwrap().iter() {
                println!("  {}", p);
            }
            std::process::exit(1);
        }
    };

    println!("\n=== Document info ===");
    println!("  title:  {:?}", doc.info.title);
    println!("  author: {:?}", doc.info.author);

    println!("\n=== metadata() payloads ===");
    use typst_library::foundations::NativeElement;
    use typst_library::introspection::MetadataElem;
    let sel = Selector::Elem(MetadataElem::ELEM, None);
    let hits = doc.introspector.query(&sel);
    println!("  ({} hits via Selector::Elem)", hits.len());
    for c in &hits {
        let label = c.label();
        let value = c.to_packed::<MetadataElem>().map(|m| &m.value);
        println!("  - label={:?} value={:?}", label, value);
    }

    println!("\n=== Asset requests ({}) ===", world.asset_log.lock().unwrap().len());
    for p in world.asset_log.lock().unwrap().iter() {
        println!("  {}", p);
    }

    println!("\n=== Rendered HTML ===");
    let html = typst_html::html(&doc).expect("html serialization");
    println!("{}", html);

    // Suppress unused-import warnings while iterating
    let _ = Prehashed::new(0u8);
    let _ = Smart::<u8>::Auto;
}

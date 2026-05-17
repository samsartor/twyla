// Experiment 2 harness — bundle mode against typst main.
//
// Goal: verify the post-PR-#7964 bundle export API. Compile `experiment.typ`
// as a `Bundle` (multi-document), then for each emitted file print:
//   - the virtual path
//   - file kind (document / asset)
//   - per-document title and metadata payloads
//   - rendered HTML (for HTML documents)
//   - asset byte length (for assets)
//
// Throwaway. None of this code lives.

use std::path::PathBuf;
use std::sync::Mutex;

use typst::diag::{FileError, FileResult, Warned};
use typst::foundations::{Bytes, Datetime, Duration, Selector};
use typst::introspection::Introspector;
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_bundle::{Bundle, BundleDocument, BundleFile};
use typst_kit::fonts::FontStore;
use typst_library::Feature;
use typst_library::foundations::NativeElement;
use typst_library::introspection::MetadataElem;
use typst_library::model::Document;

struct ExperimentWorld {
    main_id: FileId,
    source: Source,
    library: LazyHash<Library>,
    fonts: FontStore,
    asset_log: Mutex<Vec<String>>,
}

impl ExperimentWorld {
    fn new(typ_source: &str) -> Self {
        let mut fonts = FontStore::new();
        fonts.extend(typst_kit::fonts::embedded());
        fonts.extend(typst_kit::fonts::system());

        let vpath = VirtualPath::new("/main.typ").expect("valid path");
        let main_id = FileId::new(RootedPath::new(VirtualRoot::Project, vpath));
        let source = Source::new(main_id, typ_source.to_string());

        Self {
            main_id,
            source,
            library: LazyHash::new(
                Library::builder()
                    .with_features(
                        [Feature::Html, Feature::Bundle].into_iter().collect(),
                    )
                    .build(),
            ),
            fonts,
            asset_log: Mutex::new(Vec::new()),
        }
    }
}

impl World for ExperimentWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }
    fn book(&self) -> &LazyHash<FontBook> {
        self.fonts.book()
    }
    fn main(&self) -> FileId {
        self.main_id
    }
    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main_id {
            Ok(self.source.clone())
        } else {
            Err(FileError::NotFound(PathBuf::from(
                id.vpath().get_without_slash(),
            )))
        }
    }
    fn file(&self, id: FileId) -> FileResult<Bytes> {
        let path = id.vpath().get_without_slash().to_string();
        self.asset_log.lock().unwrap().push(path);
        const STUB_SVG: &[u8] =
            b"<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'></svg>";
        Ok(Bytes::new(STUB_SVG.to_vec()))
    }
    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.font(index)
    }
    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        Datetime::from_ymd(2026, 5, 17)
    }
}

fn main() {
    let typ_src = include_str!("../experiment.typ");
    let world = ExperimentWorld::new(typ_src);

    let Warned { output, warnings } = typst::compile::<Bundle>(&world);

    println!("=== Warnings ({}) ===", warnings.len());
    for w in &warnings {
        println!("  {}", w.message);
    }

    let bundle = match output {
        Ok(b) => b,
        Err(errors) => {
            println!("\n=== Errors ({}) ===", errors.len());
            for e in &errors {
                println!("  {}", e.message);
            }
            std::process::exit(1);
        }
    };

    println!("\n=== Bundle files ({}) ===", bundle.files.len());
    for (path, file) in bundle.files.iter() {
        let kind = match file {
            BundleFile::Document(BundleDocument::Html(_)) => "html",
            BundleFile::Document(BundleDocument::Paged(_, _)) => "paged",
            BundleFile::Asset(_) => "asset",
        };
        println!("  - {:?}  [{}]", path.get_without_slash(), kind);
    }

    let meta_sel = Selector::Elem(MetadataElem::ELEM, None);

    for (path, file) in bundle.files.iter() {
        let path_str = path.get_without_slash().to_string();
        println!("\n--- {} ---", path_str);
        match file {
            BundleFile::Document(BundleDocument::Html(doc)) => {
                println!("  title:  {:?}", doc.info().title);
                println!("  author: {:?}", doc.info().author);

                let hits = doc.introspector().query(&meta_sel);
                println!("  metadata in this doc: {} hits", hits.len());
                for c in &hits {
                    let label = c.label();
                    let value = c.to_packed::<MetadataElem>().map(|m| &m.value);
                    println!("    label={:?} value={:?}", label, value);
                }

                let html = typst_html::html(doc).expect("html serialization");
                println!("  --- HTML ---\n{}", html);
            }
            BundleFile::Document(BundleDocument::Paged(_, _)) => {
                println!("  (paged document, not dumped)");
            }
            BundleFile::Asset(bytes) => {
                println!("  asset bytes: {} bytes", bytes.len());
                if let Ok(s) = std::str::from_utf8(bytes) {
                    println!("  content: {:?}", s);
                }
            }
        }
    }

    println!("\n=== Bundle-wide metadata query ===");
    let hits = bundle.introspector.query(&meta_sel);
    println!("  ({} hits across whole bundle)", hits.len());
    for c in &hits {
        let label = c.label();
        let value = c.to_packed::<MetadataElem>().map(|m| &m.value);
        println!("  - label={:?} value={:?}", label, value);
    }

    println!(
        "\n=== Asset requests via World::file ({}) ===",
        world.asset_log.lock().unwrap().len()
    );
    for p in world.asset_log.lock().unwrap().iter() {
        println!("  {}", p);
    }
}

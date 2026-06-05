//! Draft generation policy — the only filesystem mutation convert performs.
//!
//! For each [`MappedPage`], decide whether to write its `.typ` neighbour based
//! on the [`ConvertMode`] and whether the file already exists. Hand-edited
//! `.typ` files are never clobbered except under [`ConvertMode::Overwrite`].

use std::fs;
use std::io;

use crate::convert::ConvertMode;
use crate::convert::report::Finding;
use crate::convert::zola::MappedPage;
use crate::import::import_md;

/// Ensure each mapped page has a `.typ` per `mode`. Returns findings describing
/// what was written or is missing.
pub fn ensure_drafts(pages: &[MappedPage], mode: ConvertMode) -> io::Result<Vec<Finding>> {
    let mut findings = Vec::new();
    for page in pages {
        let exists = page.typ_path.exists();
        match (exists, mode) {
            // Hand-edited draft already present — leave it (unless overwriting).
            (true, ConvertMode::Generate | ConvertMode::Verify) => {}
            (true, ConvertMode::Overwrite) => {
                write_draft(page)?;
                findings.push(Finding::DraftWritten {
                    typ: page.typ_path.clone(),
                });
            }
            // Missing draft in read-only mode — a completeness gap, not a write.
            (false, ConvertMode::Verify) => {
                findings.push(Finding::DraftMissing {
                    md: page.md_path.clone(),
                    route: page.route.clone(),
                });
            }
            // Missing draft — generate scaffolding.
            (false, ConvertMode::Generate | ConvertMode::Overwrite) => {
                write_draft(page)?;
                findings.push(Finding::DraftWritten {
                    typ: page.typ_path.clone(),
                });
                findings.push(Finding::Note {
                    message: format!(
                        "draft written for {} — the site won't compile until you finish it",
                        page.typ_path.display()
                    ),
                });
            }
        }
    }
    Ok(findings)
}

fn write_draft(page: &MappedPage) -> io::Result<()> {
    let src = fs::read_to_string(&page.md_path)?;
    let draft = import_md(&src, page.output_override.as_deref()).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {e}", page.md_path.display()),
        )
    })?;
    if let Some(parent) = page.typ_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&page.typ_path, draft)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn page(md: PathBuf, typ: PathBuf, output_override: Option<String>) -> MappedPage {
        MappedPage {
            md_path: md,
            typ_path: typ,
            route: "foo/index.html".to_string(),
            output_override,
        }
    }

    fn write_md(dir: &std::path::Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "+++\ntitle = \"Foo\"\n+++\nhello\n").unwrap();
        p
    }

    #[test]
    fn generate_writes_missing_with_note() {
        let dir = tempfile::tempdir().unwrap();
        let md = write_md(dir.path(), "foo.md");
        let typ = dir.path().join("foo.typ");
        let pages = vec![page(md, typ.clone(), None)];

        let findings = ensure_drafts(&pages, ConvertMode::Generate).unwrap();
        assert!(typ.exists());
        assert!(matches!(findings[0], Finding::DraftWritten { .. }));
        assert!(matches!(findings[1], Finding::Note { .. }));
        assert!(std::fs::read_to_string(&typ).unwrap().contains("page-template"));
    }

    #[test]
    fn verify_never_writes_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        let md = write_md(dir.path(), "foo.md");
        let typ = dir.path().join("foo.typ");
        let pages = vec![page(md, typ.clone(), None)];

        let findings = ensure_drafts(&pages, ConvertMode::Verify).unwrap();
        assert!(!typ.exists());
        assert!(matches!(&findings[0], Finding::DraftMissing { .. }));
    }

    #[test]
    fn generate_leaves_existing_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let md = write_md(dir.path(), "foo.md");
        let typ = dir.path().join("foo.typ");
        std::fs::write(&typ, "// hand-written\n").unwrap();
        let pages = vec![page(md, typ.clone(), None)];

        let findings = ensure_drafts(&pages, ConvertMode::Generate).unwrap();
        assert!(findings.is_empty());
        assert_eq!(std::fs::read_to_string(&typ).unwrap(), "// hand-written\n");
    }

    #[test]
    fn overwrite_clobbers_and_injects_output_override() {
        let dir = tempfile::tempdir().unwrap();
        let md = write_md(dir.path(), "foo.md");
        let typ = dir.path().join("foo.typ");
        std::fs::write(&typ, "// hand-written\n").unwrap();
        let pages = vec![page(
            md,
            typ.clone(),
            Some("foo-bar/index.html".to_string()),
        )];

        let findings = ensure_drafts(&pages, ConvertMode::Overwrite).unwrap();
        assert!(matches!(findings[0], Finding::DraftWritten { .. }));
        let written = std::fs::read_to_string(&typ).unwrap();
        assert!(written.contains(r#"#set document(output: "foo-bar/index.html")"#));
    }
}

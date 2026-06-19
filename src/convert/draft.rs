//! Draft generation policy — the only filesystem mutation convert performs.
//!
//! For each [`MappedPage`], decide whether to write its `.typ` neighbour based
//! on the [`ConvertMode`] and whether the file already exists. Hand-edited
//! `.typ` files are never clobbered except under [`ConvertMode::Overwrite`].

use std::collections::BTreeSet;
use std::fs;
use std::io;

use crate::convert::report::Finding;
use crate::convert::zola::MappedPage;
use crate::convert::{ConvertMode, SourceFormat};
use crate::import::{import_hugo_md, import_md};
use crate::project::TwylaContext;

/// Ensure each mapped page has a `.typ` per `mode`, and (when generating)
/// bootstrap a placeholder `templates/lib.typ`. Returns findings describing
/// what was written or is missing.
pub fn ensure_drafts(
    ctx: &TwylaContext,
    pages: &[MappedPage],
    mode: ConvertMode,
    source: SourceFormat,
) -> io::Result<Vec<Finding>> {
    let mut findings = Vec::new();

    // Bootstrap identity-passthrough templates so freshly-drafted pages have a
    // `{kind}-template` to import, until the real theme system lands. Never
    // clobbers an existing lib.typ; skipped in read-only Verify mode.
    if mode != ConvertMode::Verify {
        let kinds: BTreeSet<&str> = pages.iter().map(|p| p.kind.as_str()).collect();
        if let Some(f) = ensure_lib_templates(ctx, &kinds)? {
            findings.push(f);
        }
    }

    for page in pages {
        let exists = page.typ_path.exists();
        match (exists, mode) {
            // Hand-edited draft already present — leave it (unless overwriting).
            (true, ConvertMode::Generate | ConvertMode::Verify) => {}
            (true, ConvertMode::Overwrite) => {
                write_draft(page, source)?;
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
                write_draft(page, source)?;
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

fn write_draft(page: &MappedPage, source: SourceFormat) -> io::Result<()> {
    let src = fs::read_to_string(&page.md_path)?;
    let import = match source {
        SourceFormat::Zola => import_md(&src, &page.kind, page.output_override.as_deref()),
        SourceFormat::Hugo => import_hugo_md(
            &src,
            &page.kind,
            page.output_override.as_deref(),
            &page.route,
        ),
    };
    let draft = import.map_err(|e| {
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

/// Ensure `templates/lib.typ` defines an identity-passthrough
/// `{kind}-template` for every kind in use. Additive and self-healing: it
/// *appends* placeholders for kinds the file doesn't already define and never
/// touches existing content, so a kind that appears later (or a stale lib.typ
/// from an earlier run) still gets filled in. Returns a finding when it writes.
fn ensure_lib_templates(ctx: &TwylaContext, kinds: &BTreeSet<&str>) -> io::Result<Option<Finding>> {
    let lib = ctx.root.join("templates").join("lib.typ");
    let existing = fs::read_to_string(&lib).unwrap_or_default();

    let mut additions = String::new();
    for kind in kinds {
        let name = format!("{kind}-template");
        if !existing.contains(&name) {
            additions.push_str(&format!("#let {name}(body) = body\n"));
        }
    }
    if additions.is_empty() {
        return Ok(None);
    }

    let mut content = if existing.is_empty() {
        String::from(
            "// twyla placeholder templates — identity passthroughs until the\n\
             // theme system lands. Replace with real layout as you port.\n\n",
        )
    } else {
        let mut c = existing;
        if !c.ends_with('\n') {
            c.push('\n');
        }
        c
    };
    content.push_str(&additions);

    if let Some(parent) = lib.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&lib, content)?;
    Ok(Some(Finding::DraftWritten { typ: lib }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::SourceFormat;
    use std::path::{Path, PathBuf};

    fn page(md: PathBuf, typ: PathBuf, output_override: Option<String>) -> MappedPage {
        MappedPage {
            md_path: md,
            typ_path: typ,
            route: "foo/index.html".to_string(),
            kind: "page".to_string(),
            output_override,
        }
    }

    fn write_md(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "+++\ntitle = \"Foo\"\n+++\nhello\n").unwrap();
        p
    }

    /// Pre-create lib.typ so the placeholder bootstrap is a no-op (it's tested
    /// separately).
    fn with_lib(ctx: &TwylaContext) {
        let lib = ctx.root.join("templates").join("lib.typ");
        std::fs::create_dir_all(lib.parent().unwrap()).unwrap();
        std::fs::write(&lib, "#let page-template(body) = body\n").unwrap();
    }

    #[test]
    fn generate_writes_missing_with_note() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TwylaContext::new(dir.path(), None).unwrap();
        with_lib(&ctx);
        let md = write_md(dir.path(), "foo.md");
        let typ = dir.path().join("foo.typ");
        let pages = vec![page(md, typ.clone(), None)];

        let findings =
            ensure_drafts(&ctx, &pages, ConvertMode::Generate, SourceFormat::Zola).unwrap();
        assert!(typ.exists());
        assert!(matches!(findings[0], Finding::DraftWritten { .. }));
        assert!(matches!(findings[1], Finding::Note { .. }));
        let draft = std::fs::read_to_string(&typ).unwrap();
        assert!(draft.contains(r#"#import "/templates/lib.typ": *"#));
        assert!(draft.contains("#show: page-template"));
    }

    #[test]
    fn verify_never_writes_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TwylaContext::new(dir.path(), None).unwrap();
        let md = write_md(dir.path(), "foo.md");
        let typ = dir.path().join("foo.typ");
        let pages = vec![page(md, typ.clone(), None)];

        let findings =
            ensure_drafts(&ctx, &pages, ConvertMode::Verify, SourceFormat::Zola).unwrap();
        assert!(!typ.exists());
        assert!(!ctx.root.join("templates/lib.typ").exists()); // verify writes nothing
        assert!(matches!(&findings[0], Finding::DraftMissing { .. }));
    }

    #[test]
    fn generate_leaves_existing_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TwylaContext::new(dir.path(), None).unwrap();
        with_lib(&ctx);
        let md = write_md(dir.path(), "foo.md");
        let typ = dir.path().join("foo.typ");
        std::fs::write(&typ, "// hand-written\n").unwrap();
        let pages = vec![page(md, typ.clone(), None)];

        let findings =
            ensure_drafts(&ctx, &pages, ConvertMode::Generate, SourceFormat::Zola).unwrap();
        assert!(findings.is_empty());
        assert_eq!(std::fs::read_to_string(&typ).unwrap(), "// hand-written\n");
    }

    #[test]
    fn overwrite_clobbers_and_injects_output_override() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TwylaContext::new(dir.path(), None).unwrap();
        with_lib(&ctx);
        let md = write_md(dir.path(), "foo.md");
        let typ = dir.path().join("foo.typ");
        std::fs::write(&typ, "// hand-written\n").unwrap();
        let pages = vec![page(
            md,
            typ.clone(),
            Some("foo-bar/index.html".to_string()),
        )];

        let findings =
            ensure_drafts(&ctx, &pages, ConvertMode::Overwrite, SourceFormat::Zola).unwrap();
        assert!(matches!(findings[0], Finding::DraftWritten { .. }));
        let written = std::fs::read_to_string(&typ).unwrap();
        assert!(written.contains(r#"output: "foo-bar/index.html","#));
    }

    #[test]
    fn bootstraps_placeholder_lib() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = TwylaContext::new(dir.path(), None).unwrap();
        let md = write_md(dir.path(), "foo.md");
        let typ = dir.path().join("foo.typ");
        let pages = vec![page(md, typ, None)];

        ensure_drafts(&ctx, &pages, ConvertMode::Generate, SourceFormat::Zola).unwrap();
        let lib = std::fs::read_to_string(ctx.root.join("templates/lib.typ")).unwrap();
        assert!(lib.contains("#let page-template(body) = body"));
    }

    #[test]
    fn appends_missing_kind_to_existing_lib() {
        // A stale lib.typ (e.g. from a run before the kind fix) lacks
        // page-template — it should be appended, existing content preserved.
        let dir = tempfile::tempdir().unwrap();
        let ctx = TwylaContext::new(dir.path(), None).unwrap();
        let lib_path = ctx.root.join("templates").join("lib.typ");
        std::fs::create_dir_all(lib_path.parent().unwrap()).unwrap();
        std::fs::write(&lib_path, "#let dir-template(body) = [SECTION #body]\n").unwrap();

        let md = write_md(dir.path(), "foo.md");
        let typ = dir.path().join("foo.typ");
        let pages = vec![page(md, typ, None)]; // kind "page"

        ensure_drafts(&ctx, &pages, ConvertMode::Generate, SourceFormat::Zola).unwrap();
        let lib = std::fs::read_to_string(&lib_path).unwrap();
        assert!(lib.contains("#let dir-template(body) = [SECTION #body]")); // preserved
        assert!(lib.contains("#let page-template(body) = body")); // appended
    }
}

//! `twyla build` — compile every page and write a static site to disk.
//!
//! Output layout matches what the dev server serves at runtime:
//!
//! - One `<slug>/index.html` per routed page (whatever
//!   [`render_site`](crate::render::render_site) produces).
//! - Everything under `<site>/static/` copied verbatim.
//! - Colocated content assets — non-`.typ` files under `<site>/content/` —
//!   copied to the output root. Mirrors zola's behavior (and what the
//!   dev server falls back to). Goes away when twyla has a real asset
//!   pipeline.
//!
//! No cleaning of the output dir, no manifest, no dependency tracking.
//! Stale files from prior builds remain unless you `rm -rf` first.
//! Revisit once we're confident about a stable dir layout.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::project::TwylaContext;
use crate::render::{RenderError, render_site};

pub struct Build {
    pub ctx: TwylaContext,
    pub output_dir: PathBuf,
}

#[derive(Debug)]
pub enum BuildError {
    Render(RenderError),
    Io { context: String, source: io::Error },
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Render(e) => write!(f, "{e}"),
            Self::Io { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl std::error::Error for BuildError {}

pub fn run(build: Build) -> Result<BuildSummary, BuildError> {
    let docs = render_site(&build.ctx).map_err(BuildError::Render)?;

    fs::create_dir_all(&build.output_dir).map_err(|e| BuildError::Io {
        context: format!("creating {}", build.output_dir.display()),
        source: e,
    })?;

    for doc in &docs {
        let dest = build.output_dir.join(&doc.path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| BuildError::Io {
                context: format!("creating {}", parent.display()),
                source: e,
            })?;
        }
        fs::write(&dest, &doc.html).map_err(|e| BuildError::Io {
            context: format!("writing {}", dest.display()),
            source: e,
        })?;
    }

    let static_dir = build.ctx.static_dir();
    let static_copied = if static_dir.is_dir() {
        copy_dir_contents(&static_dir, &build.output_dir)?
    } else {
        0
    };

    // Zola-style colocated content assets: anything under `content/`
    // that isn't a `.typ` file gets emitted at its root path.
    let content_dir = build.ctx.content_dir();
    let content_copied = if content_dir.is_dir() {
        copy_non_typ_contents(&content_dir, &build.output_dir)?
    } else {
        0
    };

    Ok(BuildSummary {
        pages: docs.len(),
        static_files: static_copied,
        content_assets: content_copied,
    })
}

#[derive(Debug, Clone, Copy)]
pub struct BuildSummary {
    pub pages: usize,
    pub static_files: usize,
    pub content_assets: usize,
}

/// Recursive copy of every file under `src` into `dst`, mirroring the
/// directory tree. Follows symlinks (we want the symlink *targets*
/// landing in the output, not the symlinks themselves).
fn copy_dir_contents(src: &Path, dst: &Path) -> Result<usize, BuildError> {
    let entries = fs::read_dir(src).map_err(|e| BuildError::Io {
        context: format!("reading {}", src.display()),
        source: e,
    })?;
    let mut count = 0;
    for entry in entries {
        let entry = entry.map_err(|e| BuildError::Io {
            context: format!("reading entry under {}", src.display()),
            source: e,
        })?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        // `metadata()` follows symlinks — we want target type, not link type.
        let meta = fs::metadata(&from).map_err(|e| BuildError::Io {
            context: format!("statting {}", from.display()),
            source: e,
        })?;
        if meta.is_dir() {
            fs::create_dir_all(&to).map_err(|e| BuildError::Io {
                context: format!("creating {}", to.display()),
                source: e,
            })?;
            count += copy_dir_contents(&from, &to)?;
        } else if meta.is_file() {
            fs::copy(&from, &to).map_err(|e| BuildError::Io {
                context: format!("copying {} → {}", from.display(), to.display()),
                source: e,
            })?;
            count += 1;
        }
    }
    Ok(count)
}

/// Like [`copy_dir_contents`] but skips source-format files — `.typ`
/// (twyla pages) and `.md` (zola legacy still on disk during porting).
/// Used for the colocated-asset bridge.
fn copy_non_typ_contents(
    src: &Path,
    dst: &Path,
) -> Result<usize, BuildError> {
    let entries = fs::read_dir(src).map_err(|e| BuildError::Io {
        context: format!("reading {}", src.display()),
        source: e,
    })?;
    let mut count = 0;
    for entry in entries {
        let entry = entry.map_err(|e| BuildError::Io {
            context: format!("reading entry under {}", src.display()),
            source: e,
        })?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = fs::metadata(&from).map_err(|e| BuildError::Io {
            context: format!("statting {}", from.display()),
            source: e,
        })?;
        if meta.is_dir() {
            fs::create_dir_all(&to).map_err(|e| BuildError::Io {
                context: format!("creating {}", to.display()),
                source: e,
            })?;
            count += copy_non_typ_contents(&from, &to)?;
        } else if meta.is_file() {
            let ext = from.extension().and_then(|s| s.to_str());
            if matches!(ext, Some("typ") | Some("md")) {
                continue;
            }
            fs::copy(&from, &to).map_err(|e| BuildError::Io {
                context: format!("copying {} → {}", from.display(), to.display()),
                source: e,
            })?;
            count += 1;
        }
    }
    Ok(count)
}

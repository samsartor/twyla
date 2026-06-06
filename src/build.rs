//! `twyla build` — compile every page and write a static site to disk.
//!
//! Output layout matches what the dev server serves at runtime:
//!
//! - One `<slug>/index.html` per routed page (whatever
//!   [`render_site`](crate::render::render_site) produces).
//! - Every processed (`asset.*`) asset under `assets/`.
//! - Each [`copy root`](TwylaContext::copy_roots) — `static/` verbatim, plus
//!   colocated `content/` assets when enabled.
//!
//! The set of files written is [`emit_plan`] — the single enumeration the
//! twyla manifest ([`crate::convert::manifest::twyla`]) also consumes, so the
//! build and the manifest can never disagree about what ships.
//!
//! No cleaning of the output dir, no dependency tracking. Stale files from
//! prior builds remain unless you `rm -rf` first. Revisit once we're confident
//! about a stable dir layout.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use typst::foundations::Bytes;

use crate::project::TwylaContext;
use crate::render::{RenderError, SiteOutput, render_site_with_assets};

pub struct Build {
    pub ctx: TwylaContext,
    pub output_dir: PathBuf,
}

/// One output file's contents and provenance — the currency of the build plan.
/// One variant per kind of thing the build produces, so consumers (the writer,
/// the manifest, the summary) can both *emit* it and *classify* it without a
/// second lookup. `*Copy` variants keep only the source path (stream-copied at
/// emit time, never the whole file in memory); the rest hold in-memory bytes.
#[derive(Clone, Debug)]
pub enum Emit {
    /// A compiled page's rendered HTML.
    Doc(Bytes),
    /// A processed asset's transformed bytes (e.g. compiled CSS).
    AssetBytes(Bytes),
    /// A processed asset stream-copied verbatim from its on-disk source.
    AssetCopy(PathBuf),
    /// A verbatim file from a [`copy root`](TwylaContext::copy_roots)
    /// (`static/` or, when enabled, colocated `content/`).
    CopyRoot(PathBuf),
}

impl Emit {
    /// Write this output to `dest` — `fs::write` for in-memory bytes, a
    /// stream `fs::copy` for the path-backed variants.
    fn write_to(&self, dest: &Path) -> io::Result<()> {
        match self {
            Emit::Doc(bytes) | Emit::AssetBytes(bytes) => fs::write(dest, bytes),
            Emit::AssetCopy(src) | Emit::CopyRoot(src) => fs::copy(src, dest).map(drop),
        }
    }
}

/// One output file: its root-relative `/`-separated path and how its bytes
/// reach disk. The unit of [`emit_plan`].
pub struct EmitEntry {
    pub path: String,
    pub emit: Emit,
}

/// Enumerate every file a `twyla build` emits: compiled pages, processed
/// assets, then each [`copy root`](TwylaContext::copy_roots). The authority
/// both [`run`] (which writes the bytes) and [`crate::convert::manifest::twyla`]
/// (which lists the paths) consume — one source of truth for "what ships."
pub fn emit_plan(ctx: &TwylaContext, site: &SiteOutput) -> io::Result<Vec<EmitEntry>> {
    let mut plan = Vec::new();
    for doc in &site.docs {
        plan.push(EmitEntry {
            path: crate::project::path_key(&doc.path),
            emit: Emit::Doc(Bytes::new(doc.html.clone().into_bytes())),
        });
    }
    for asset in &site.assets {
        plan.push(EmitEntry {
            path: asset.output_path.clone(),
            emit: asset.built.emit.clone(),
        });
    }
    for root in ctx.copy_roots() {
        for (key, src) in root.walk()? {
            plan.push(EmitEntry {
                path: key,
                emit: Emit::CopyRoot(src),
            });
        }
    }
    Ok(plan)
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
    let site = render_site_with_assets(&build.ctx).map_err(BuildError::Render)?;

    fs::create_dir_all(&build.output_dir).map_err(|e| BuildError::Io {
        context: format!("creating {}", build.output_dir.display()),
        source: e,
    })?;

    let plan = emit_plan(&build.ctx, &site).map_err(|e| BuildError::Io {
        context: "enumerating build outputs".to_string(),
        source: e,
    })?;

    let mut summary = BuildSummary::default();
    for entry in &plan {
        let dest = build.output_dir.join(&entry.path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| BuildError::Io {
                context: format!("creating {}", parent.display()),
                source: e,
            })?;
        }
        entry.emit.write_to(&dest).map_err(|e| BuildError::Io {
            context: format!("emitting {}", dest.display()),
            source: e,
        })?;
        match entry.emit {
            Emit::Doc(_) => summary.pages += 1,
            Emit::AssetBytes(_) | Emit::AssetCopy(_) => summary.assets += 1,
            Emit::CopyRoot(_) => summary.copied += 1,
        }
    }

    Ok(summary)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BuildSummary {
    pub pages: usize,
    /// Processed (`asset.*`) assets emitted under `assets/`.
    pub assets: usize,
    /// Verbatim files copied from the [`copy roots`](TwylaContext::copy_roots)
    /// (`static/` + optional colocated content).
    pub copied: usize,
}

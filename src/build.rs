//! `twyla build` — compile every page and write a static site to disk.
//!
//! Output layout matches what the dev server serves at runtime:
//!
//! - One `<slug>/index.html` per routed page.
//! - Every processed (`asset.*`) asset under `assets/`.
//! - Every `static/` file, verbatim.
//!
//! All three come from one [`Outputs`](crate::render::Outputs) map — the same
//! object the twyla manifest enumerates and the dev server serves from — so
//! the build can't disagree with them about what ships.
//!
//! No cleaning of the output dir, no dependency tracking. Stale files from
//! prior builds remain unless you `rm -rf` first. Revisit once we're confident
//! about a stable dir layout.

use std::io;
use std::path::PathBuf;

use crate::project::TwylaContext;
use crate::render::{Output, Outputs, RenderError, render_site};

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
    let outputs = render_site(&build.ctx).map_err(BuildError::Render)?;
    outputs
        .emit_to_fs(&build.output_dir)
        .map_err(|e| BuildError::Io {
            context: "writing site outputs".to_string(),
            source: e,
        })?;
    Ok(BuildSummary::of(&outputs))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BuildSummary {
    pub pages: usize,
    pub assets: usize,
    pub static_files: usize,
}

impl BuildSummary {
    /// Tally the outputs by kind for the post-build report line.
    fn of(outputs: &Outputs) -> Self {
        let mut s = Self::default();
        for output in outputs.iter() {
            match output {
                Output::Doc(_) => s.pages += 1,
                Output::Asset(_) => s.assets += 1,
                Output::Static(..) => s.static_files += 1,
            }
        }
        s
    }
}

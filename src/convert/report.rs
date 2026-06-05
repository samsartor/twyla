//! The unified finding list and its rendering.
//!
//! Every phase of a convert run — draft generation, per-page diff, the link
//! audit, the completeness check — pushes [`Finding`]s into one flat list,
//! which [`render`] prints (colored, when stdout is a tty) followed by a single
//! `RESULT:` line.

use std::fmt::Write as _;
use std::path::PathBuf;

/// How a finding affects the run's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Informational — never affects the exit code (drafts written, page pass).
    Info,
    /// Something to look at, but not a failure (e.g. a route twyla invented).
    Warn,
    /// A real failure — forces a non-zero exit.
    Fail,
}

/// One observation from a convert run.
#[derive(Debug, Clone)]
pub enum Finding {
    /// A draft `.typ` was written to disk.
    DraftWritten { typ: PathBuf },
    /// A page has no `.typ` and we won't generate one (`--verify`).
    DraftMissing { md: PathBuf, route: String },
    /// A general note (e.g. "site won't compile until you finish draft X").
    Note { message: String },
    /// A page diffed clean against the ground truth.
    PagePass { route: String },
    /// A page diverged from the ground truth; `divergence` is the formatted
    /// first divergence.
    PageDiff { route: String, divergence: String },
    /// A URL on a twyla page doesn't resolve in twyla's own manifest
    /// (self-containment; `navigable` marks broken `<a>` links specifically).
    Unreachable {
        page: String,
        url: String,
        navigable: bool,
    },
    /// A navigable URL the ground truth exposes is no longer produced by twyla.
    NavigableDropped {
        page: String,
        url: String,
        hint: String,
    },
    /// An expected route wasn't produced by twyla.
    RouteMissing { route: String },
    /// Twyla produced a route with no ground-truth counterpart.
    RouteExtra { route: String },
}

impl Finding {
    pub fn severity(&self) -> Severity {
        match self {
            Finding::DraftWritten { .. } | Finding::PagePass { .. } => Severity::Info,
            Finding::Note { .. } | Finding::RouteExtra { .. } => Severity::Warn,
            Finding::DraftMissing { .. }
            | Finding::PageDiff { .. }
            | Finding::Unreachable { .. }
            | Finding::NavigableDropped { .. }
            | Finding::RouteMissing { .. } => Severity::Fail,
        }
    }

    /// A short bracketed tag for the rendered line.
    fn tag(&self) -> &'static str {
        match self {
            Finding::DraftWritten { .. } => "draft",
            Finding::DraftMissing { .. } => "missing-draft",
            Finding::Note { .. } => "note",
            Finding::PagePass { .. } => "pass",
            Finding::PageDiff { .. } => "diff",
            Finding::Unreachable { navigable, .. } => {
                if *navigable {
                    "broken-link"
                } else {
                    "unreachable"
                }
            }
            Finding::NavigableDropped { .. } => "url-dropped",
            Finding::RouteMissing { .. } => "missing-route",
            Finding::RouteExtra { .. } => "extra-route",
        }
    }

    /// The human-readable body, possibly multi-line (indented by the renderer).
    fn body(&self) -> String {
        match self {
            Finding::DraftWritten { typ } => format!("wrote {}", typ.display()),
            Finding::DraftMissing { md, route } => {
                format!("{} has no .typ (route {route})", md.display())
            }
            Finding::Note { message } => message.clone(),
            Finding::PagePass { route } => route.clone(),
            Finding::PageDiff { route, divergence } => format!("{route}\n{divergence}"),
            Finding::Unreachable { page, url, .. } => {
                format!("{page}: {url} — not in twyla's manifest (never satisfied by public/)")
            }
            Finding::NavigableDropped { page, url, hint } => {
                format!("{page}: {url}\n{hint}")
            }
            Finding::RouteMissing { route } => format!("{route} — expected, not produced by twyla"),
            Finding::RouteExtra { route } => format!("{route} — produced, no ground-truth peer"),
        }
    }
}

/// Whether any finding is a hard failure (drives the exit code).
pub fn has_failure(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.severity() == Severity::Fail)
}

/// Render the findings to a string. `color` enables ANSI escapes (the CLI
/// passes the result of a tty/`NO_COLOR` check).
pub fn render(findings: &[Finding], color: bool) -> String {
    let mut out = String::new();
    let (mut fails, mut warns) = (0u32, 0u32);
    for f in findings {
        let sev = f.severity();
        match sev {
            Severity::Fail => fails += 1,
            Severity::Warn => warns += 1,
            Severity::Info => {}
        }
        let tag = paint(f.tag(), sev, color);
        let body = f.body();
        let mut lines = body.lines();
        let first = lines.next().unwrap_or("");
        writeln!(out, "  {tag} {first}").unwrap();
        for line in lines {
            writeln!(out, "        {line}").unwrap();
        }
    }
    let result = if fails > 0 {
        paint(&format!("FAIL ({fails} failures, {warns} warnings)"), Severity::Fail, color)
    } else if warns > 0 {
        paint(&format!("OK ({warns} warnings)"), Severity::Warn, color)
    } else {
        paint("OK", Severity::Info, color)
    };
    writeln!(out, "\nRESULT: {result}").unwrap();
    out
}

fn paint(s: &str, sev: Severity, color: bool) -> String {
    if !color {
        return s.to_string();
    }
    use owo_colors::{OwoColorize, Style};
    let style = match sev {
        Severity::Info => Style::new().green(),
        Severity::Warn => Style::new().yellow(),
        Severity::Fail => Style::new().red().bold(),
    };
    s.style(style).to_string()
}

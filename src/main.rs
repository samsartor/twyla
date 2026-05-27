//! `twyla` — the CLI.
//!
//! Two audiences. End-user (zero-flag, cwd-driven):
//!
//! - `twyla serve` — dev server on port 1111.
//!
//! Porting harness (flag-driven):
//!
//! - `twyla render [--site-root <dir>] <slug>` — compile a single page.
//! - `twyla check  [--site-root <dir>] <slug>` — render + diff against
//!   `<site>/public/<slug>/index.html` under the porting relaxations.
//! - `twyla diff   <expected> <actual>` — structural AST diff.
//! - `twyla import <md>` — md→typ draft generator.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use twyla::diff::{Matcher, RelaxConfig, RelaxationRule, diff, parse_html};
use twyla::import::import_md;
use twyla::render::render_slug;
use twyla::serve::{Serve, run as serve_run};

#[derive(Parser)]
#[command(
    name = "twyla",
    version,
    about = "typst-based SSG with porting harness"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the dev server. Zero flags — operates on the current
    /// working directory. Binds to port 1111.
    Serve,
    /// Compile a single page, run the resolution pass, print HTML.
    Render {
        /// Site repo root. Defaults to `$SITE_ROOT` then `$HOME/Src/site`.
        #[arg(long, env = "SITE_ROOT")]
        site_root: Option<PathBuf>,
        /// Page slug — `guis-2` for `content/guis-2.typ`.
        slug: String,
    },
    /// Structurally diff two HTML files.
    Diff {
        /// Relax `<pre>` blocks to text-only equality.
        #[arg(long)]
        textonly_pre: bool,
        /// Ignore attribute on tag, format `<tag>:<attr>`. Repeatable.
        #[arg(long, value_name = "TAG:ATTR")]
        ignore_attr: Vec<String>,
        expected: PathBuf,
        actual: PathBuf,
    },
    /// Render a ported page and diff it against the zola-built version.
    ///
    /// Zola's content/public layout is the manifest: given a slug, the
    /// inputs are `<site>/content/<slug>.typ` and
    /// `<site>/public/<slug>/index.html`. No per-page configuration today.
    Check {
        /// Site repo root (zola side). Defaults to `$SITE_ROOT` then
        /// `$HOME/Src/site`.
        #[arg(long, env = "SITE_ROOT")]
        site_root: Option<PathBuf>,
        /// Page slug — `guis-2` for `content/guis-2.typ` and
        /// `public/guis-2/index.html`.
        slug: String,
    },
    /// Convert a zola markdown post to a typst draft on stdout.
    ///
    /// Best-effort scaffolding — handles the common shape of the
    /// personal-site corpus (frontmatter, headings, paragraphs,
    /// blockquotes, fenced code, links, centered/svg/image/diagram
    /// shortcodes). Manual cleanup is expected for raw HTML, custom
    /// shortcodes, and per-page link quirks. Pipe through `> foo.typ`
    /// and iterate against `twyla check`.
    Import {
        /// Path to the source markdown file.
        input: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve => cmd_serve(),
        Cmd::Render { site_root, slug } => cmd_render(site_root, &slug),
        Cmd::Diff { textonly_pre, ignore_attr, expected, actual } => {
            cmd_diff(textonly_pre, &ignore_attr, &expected, &actual)
        }
        Cmd::Check { site_root, slug } => cmd_check(site_root, &slug),
        Cmd::Import { input } => cmd_import(&input),
    }
}

fn cmd_serve() -> ExitCode {
    let site_root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot read current directory: {e}");
            return ExitCode::from(2);
        }
    };
    if !site_root.join("content").is_dir() {
        eprintln!(
            "error: no `content/` directory under {} — run `twyla serve` \
             from the project root.",
            site_root.display(),
        );
        return ExitCode::from(2);
    }
    let addr: SocketAddr = "127.0.0.1:1111".parse().unwrap();
    match serve_run(Serve { site_root, addr }) {
        Ok(()) => ExitCode::from(0),
        Err(e) => {
            eprintln!("serve error: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_import(input: &Path) -> ExitCode {
    let src = match std::fs::read_to_string(input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading {}: {e}", input.display());
            return ExitCode::from(2);
        }
    };
    match import_md(&src) {
        Ok(out) => {
            print!("{out}");
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("import error: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_render(site_root: Option<PathBuf>, slug: &str) -> ExitCode {
    let site_root = match resolve_site_root(site_root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    match render_slug(&site_root, slug) {
        Ok(doc) => {
            print!("{}", doc.html);
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_diff(
    textonly_pre: bool,
    ignore_attr: &[String],
    expected_path: &Path,
    actual_path: &Path,
) -> ExitCode {
    let mut cfg = RelaxConfig::new();
    if textonly_pre {
        cfg = cfg.relax(Matcher::Tag("pre".to_string()), RelaxationRule::TextOnly);
    }
    for spec in ignore_attr {
        let Some((tag, attr)) = spec.split_once(':') else {
            eprintln!("--ignore-attr expects <tag>:<attr>, got {spec:?}");
            return ExitCode::from(2);
        };
        cfg = cfg.relax(
            Matcher::Tag(tag.to_string()),
            RelaxationRule::IgnoreAttribute(attr.to_string()),
        );
    }

    let expected_src = match std::fs::read_to_string(expected_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading {}: {e}", expected_path.display());
            return ExitCode::from(2);
        }
    };
    let actual_src = match std::fs::read_to_string(actual_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading {}: {e}", actual_path.display());
            return ExitCode::from(2);
        }
    };

    let expected = parse_html(&expected_src);
    let actual = parse_html(&actual_src);

    match diff(&expected, &actual, &cfg) {
        Ok(()) => {
            println!(
                "match: {} == {}",
                expected_path.display(),
                actual_path.display()
            );
            ExitCode::from(0)
        }
        Err(d) => {
            println!("{d}");
            ExitCode::from(1)
        }
    }
}

/// Render `<site>/content/<slug>.typ` and diff against
/// `<site>/public/<slug>/index.html` under the porting relaxations.
///
/// Relaxations applied are the cumulative set found necessary across pages
/// ported so far. New ones get added here (after Sam-approves the
/// divergence) until enough pages need page-specific overrides to justify
/// a manifest.
fn cmd_check(site_root: Option<PathBuf>, slug: &str) -> ExitCode {
    let site_root = match resolve_site_root(site_root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    let zola_html_path = site_root.join(format!("public/{slug}/index.html"));

    eprintln!(">>> rendering {slug}");
    let typst_html = match render_slug(&site_root, slug) {
        Ok(doc) => doc.html,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    let zola_html = match std::fs::read_to_string(&zola_html_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading {}: {e}", zola_html_path.display());
            return ExitCode::from(2);
        }
    };

    // Universal relaxations — each is a known structural divergence
    // between typst's HTML export and zola's pulldown+tera output. See
    // `doc/index.typ` § Lessons for the full reasoning per item.
    // - `<pre>`: zola syntect spans vs typst's verbatim text body.
    // - `td`/`th` `style`: typst `#table(align: ..)` is layout-only and
    //   doesn't reflect into per-cell `style="text-align:.."`.
    let cfg = RelaxConfig::new()
        .relax(Matcher::Tag("pre".to_string()), RelaxationRule::TextOnly)
        .relax(
            Matcher::Tag("td".to_string()),
            RelaxationRule::IgnoreAttribute("style".to_string()),
        )
        .relax(
            Matcher::Tag("th".to_string()),
            RelaxationRule::IgnoreAttribute("style".to_string()),
        );

    let expected = parse_html(&zola_html);
    let actual = parse_html(&typst_html);

    eprintln!(">>> diff");
    match diff(&expected, &actual, &cfg) {
        Ok(()) => {
            println!("match: {} == typst render", zola_html_path.display());
            ExitCode::from(0)
        }
        Err(d) => {
            println!("{d}");
            ExitCode::from(1)
        }
    }
}

fn resolve_site_root(arg: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(p) = arg {
        return Ok(p);
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "neither --site-root nor $SITE_ROOT nor $HOME is set".to_string())?;
    Ok(PathBuf::from(home).join("Src/site"))
}

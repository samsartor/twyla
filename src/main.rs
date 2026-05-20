//! `twyla` — the CLI.
//!
//! Subcommands:
//!
//! - `twyla render [--root <dir>] <entrypoint.typ>` — compile and print
//!   resolved HTML to stdout.
//! - `twyla diff [--textonly-pre] [--ignore-attr <tag>:<attr>]...
//!   <expected.html> <actual.html>` — structural AST diff.
//! - `twyla port [--site-root <dir>]` — render `<site>/content/guis-2.typ`
//!   and diff against `<site>/public/guis-2/index.html` under the porting
//!   relaxations. Hardcoded to guis-2 today; grows arguments when more
//!   pages land.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use twyla::diff::{Matcher, RelaxConfig, RelaxationRule, diff, parse_html};
use twyla::render::render_to_html;

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
    /// Compile a typst entrypoint, run the resolution pass, print HTML.
    Render {
        /// Typst project root (`/foo.typ` resolves here). Defaults to
        /// the entrypoint's parent — usually wrong for any real project,
        /// so pass `--root` explicitly.
        #[arg(long)]
        root: Option<PathBuf>,
        entrypoint: PathBuf,
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
    /// Render guis-2.typ and diff against the zola-built version.
    Port {
        /// Site repo root (zola side). Defaults to `$SITE_ROOT` then
        /// `$HOME/Src/site`.
        #[arg(long, env = "SITE_ROOT")]
        site_root: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Render { root, entrypoint } => cmd_render(root.as_deref(), &entrypoint),
        Cmd::Diff { textonly_pre, ignore_attr, expected, actual } => {
            cmd_diff(textonly_pre, &ignore_attr, &expected, &actual)
        }
        Cmd::Port { site_root } => cmd_port(site_root),
    }
}

fn cmd_render(root: Option<&Path>, entrypoint: &Path) -> ExitCode {
    let resolved_root = match root {
        Some(r) => r.to_path_buf(),
        None => match entrypoint.parent() {
            Some(p) => p.to_path_buf(),
            None => {
                eprintln!("error: entrypoint has no parent directory");
                return ExitCode::from(2);
            }
        },
    };

    match render_to_html(&resolved_root, entrypoint) {
        Ok(html) => {
            print!("{html}");
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

/// `port-page.sh`'s workflow, in-process. Today: guis-2 only — entrypoint
/// path, zola output path, and relaxations are hardcoded. When we port a
/// second page these become arguments.
fn cmd_port(site_root: Option<PathBuf>) -> ExitCode {
    let site_root = match resolve_site_root(site_root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    let entrypoint = site_root.join("content/guis-2.typ");
    let zola_html_path = site_root.join("public/guis-2/index.html");

    eprintln!(">>> rendering {}", entrypoint.display());
    let typst_html = match render_to_html(&site_root, &entrypoint) {
        Ok(s) => s,
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

    // Hardcoded porting relaxations for guis-2.
    // - `<pre>` blocks differ in syntax-highlighter span structure but
    //   match as concatenated text (see `doc/index.typ` § Lessons).
    // - typst `#table(align: ..)` is layout-only; zola emits per-cell
    //   `style="text-align:.."`. Ignore that style on td/th.
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

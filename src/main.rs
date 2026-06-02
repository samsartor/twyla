//! `twyla` — the CLI.
//!
//! Every subcommand resolves a [`TwylaContext`] from the shared
//! `#[command(flatten)] ContextArgs` (defaulting to cwd) and hands it
//! to the library. Two audiences:
//!
//! End-user (cwd-driven, no flags required):
//!
//! - `twyla serve` — dev server on port 1111.
//! - `twyla build [-o <dir>]` — write the static site to `./public/`
//!   (or wherever `-o` points).
//!
//! Porting harness:
//!
//! - `twyla render <slug>` — compile a single page.
//! - `twyla check  <slug>` — render + diff against
//!   `<root>/public/<slug>/index.html` under the porting relaxations.
//!   Requires `--base-url` (or `TWYLA_BASE_URL`) for the
//!   anchor-link rewrite.
//! - `twyla diff <expected> <actual>` — structural AST diff.
//! - `twyla import <md>` — md→typ draft generator.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use twyla::build::{Build, run as build_run};
use twyla::diff::{Matcher, RelaxConfig, RelaxationRule, diff, parse_html};
use twyla::import::import_md;
use twyla::project::TwylaContext;
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

/// Common project-resolution args. Flattened into every subcommand
/// that operates on a twyla project. Today: `--root` and
/// `--base-url`. Future: a `--config <PATH>` for a `twyla.toml`.
#[derive(Args, Clone, Debug)]
struct ContextArgs {
    /// Project root. Defaults to the current directory.
    #[arg(long, env = "TWYLA_ROOT", global = true)]
    root: Option<PathBuf>,
    /// Base URL (origin + optional path prefix). Required by `check`
    /// for anchor-link rewriting; optional elsewhere until typst-side
    /// asset URLs land.
    #[arg(long, env = "TWYLA_BASE_URL", global = true)]
    base_url: Option<String>,
}

impl ContextArgs {
    /// Resolve into a [`TwylaContext`]. Falls back to cwd when `--root`
    /// is absent; verifies `content/` exists before returning so
    /// downstream errors are about real problems rather than wrong cwd.
    fn resolve(self) -> Result<TwylaContext, String> {
        let root = match self.root {
            Some(p) => p,
            None => std::env::current_dir()
                .map_err(|e| format!("cannot read current directory: {e}"))?,
        };
        let ctx = TwylaContext::new(&root, self.base_url)?;
        if !ctx.content_dir().is_dir() {
            return Err(format!(
                "no `content/` directory under {} — run twyla from \
                 the project root, or pass --root <PATH>.",
                ctx.root.display(),
            ));
        }
        Ok(ctx)
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the dev server. Zero flags — operates on the current
    /// working directory (or `--root <PATH>`). Binds to port 1111.
    Serve {
        #[command(flatten)]
        ctx: ContextArgs,
    },
    /// Compile every page under `content/` and write the static site
    /// to disk. Default output `<root>/public/` (matches zola).
    Build {
        #[command(flatten)]
        ctx: ContextArgs,
        /// Output directory. Defaults to `<root>/public/`
        /// (zola-compatible). Existing files are overwritten in place;
        /// nothing is removed.
        #[arg(long, short = 'o')]
        output_dir: Option<PathBuf>,
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
    /// inputs are `<root>/content/<slug>.typ` and
    /// `<root>/public/<slug>/index.html`. Requires `--base-url` for
    /// reconciling zola's absolutized anchor links against twyla's
    /// fragment-only form.
    Check {
        #[command(flatten)]
        ctx: ContextArgs,
        /// Page slug — `guis-2` for `content/guis-2.typ` and
        /// `public/guis-2/index.html`.
        slug: String,
    },
    /// Convert a zola markdown post to a typst draft on stdout.
    Import {
        /// Path to the source markdown file.
        input: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { ctx } => cmd_serve(ctx),
        Cmd::Build { ctx, output_dir } => cmd_build(ctx, output_dir),
        Cmd::Diff {
            textonly_pre,
            ignore_attr,
            expected,
            actual,
        } => cmd_diff(textonly_pre, &ignore_attr, &expected, &actual),
        Cmd::Check { ctx, slug } => cmd_check(ctx, &slug),
        Cmd::Import { input } => cmd_import(&input),
    }
}

/// Resolve `ContextArgs` or return ExitCode 2 with the error printed.
fn resolve_ctx(args: ContextArgs) -> Result<TwylaContext, ExitCode> {
    match args.resolve() {
        Ok(c) => Ok(c),
        Err(e) => {
            eprintln!("{e}");
            Err(ExitCode::from(2))
        }
    }
}

fn cmd_build(args: ContextArgs, output_dir: Option<PathBuf>) -> ExitCode {
    let ctx = match resolve_ctx(args) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let output_dir = output_dir.unwrap_or_else(|| ctx.default_output_dir());
    let start = std::time::Instant::now();
    match build_run(Build {
        ctx,
        output_dir: output_dir.clone(),
    }) {
        Ok(summary) => {
            eprintln!(
                "twyla build: {} pages, {} assets, {} static, {} colocated → {} ({:.1?})",
                summary.pages,
                summary.assets,
                summary.static_files,
                summary.content_assets,
                output_dir.display(),
                start.elapsed(),
            );
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("build error: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_serve(args: ContextArgs) -> ExitCode {
    let ctx = match resolve_ctx(args) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let addr: SocketAddr = "127.0.0.1:1111".parse().unwrap();
    match serve_run(Serve { ctx, addr }) {
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

/// Render `<root>/content/<slug>.typ` and diff against
/// `<root>/public/<slug>/index.html` under the porting relaxations.
///
/// Requires `--base-url` (or `TWYLA_BASE_URL`) for
/// `rewrite_own_page_anchor_hrefs` — zola absolutizes anchor-only
/// links against the base, typst emits fragment-only, so we rewrite
/// zola's form back before comparison.
fn cmd_check(_args: ContextArgs, _slug: &str) -> ExitCode {
    todo!("revive the check function")
    /*
    let ctx = match resolve_ctx(args) {
        Ok(c) => c,
        Err(code) => return code,
    };

    let base_url = match ctx.require_base_url() {
        Ok(u) => u.to_string(),
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    let zola_html_path = ctx.root.join(format!("public/{slug}/index.html"));

    eprintln!(">>> rendering {slug}");
    let typst_html = match render_path(&ctx, slug) {
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

    let mut expected = parse_html(&zola_html);
    // Zola absolutizes anchor-only links (`[t](#frag)`) against the
    // page's base URL: `<a href="<base>/<slug>/#frag">`. Typst's
    // label-based `#link(<frag>)` emits the unabsolutized form
    // `<a href="#frag">`. Both resolve to the same target, so for
    // diff purposes rewrite the zola form back to the fragment-only
    // form before comparison.
    let route_url = ctx.default_route(slug).url_path;
    rewrite_own_page_anchor_hrefs(&mut expected, &format!("{base_url}{route_url}#"));
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
    */
}

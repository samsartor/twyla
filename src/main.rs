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
//! - `twyla convert --from zola` — discover markdown, generate the missing
//!   typst neighbours, compile the whole site, and diff + link-audit it against
//!   the zola ground truth (`public/`). `--verify` is the read-only gate;
//!   `--only <slug>` scopes to one page. Requires `--base-url` (or
//!   `TWYLA_BASE_URL`) for the link audit and anchor-link rewrite.
//! - `twyla import <md>` — md→typ draft generator (primitive).

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

use twyla::build::{Build, run as build_run};
use twyla::convert::{self, ConvertMode, ConvertOptions};
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
    /// Port a markdown site: generate the missing typst drafts, compile the
    /// whole site, and diff + link-audit it against the zola ground truth.
    ///
    /// Requires `--base-url` (or `TWYLA_BASE_URL`) for the link audit and the
    /// anchor-link rewrite.
    Convert {
        #[command(flatten)]
        ctx: ContextArgs,
        /// Source format. Today only `zola`.
        #[arg(long = "from", default_value = "zola")]
        from: FromFormat,
        /// Regenerate every draft, clobbering existing `.typ` files.
        #[arg(long, conflicts_with = "verify")]
        overwrite: bool,
        /// Never generate; diff + audit only (the read-only CI/skill gate).
        #[arg(long)]
        verify: bool,
        /// Also write twyla's compiled HTML here for inspection.
        #[arg(long)]
        output_dir: Option<PathBuf>,
        /// Ground-truth dir to diff against. Defaults to `<root>/public/`.
        #[arg(long)]
        ground_truth: Option<PathBuf>,
        /// Scope diff + audit + completeness to a single route slug.
        #[arg(long)]
        only: Option<String>,
        /// Sibling nodes of context to show above a divergence in the diff.
        #[arg(short = 'A', long, default_value_t = 1)]
        above: usize,
        /// Sibling nodes of context to show below a divergence in the diff.
        #[arg(short = 'B', long, default_value_t = 1)]
        below: usize,
        /// Show every difference per page, not just the first.
        #[arg(long)]
        all_diffs: bool,
    },
    /// Convert a zola markdown post to a typst draft on stdout.
    Import {
        /// Path to the source markdown file.
        input: PathBuf,
    },
}

/// Source site format for `twyla convert`.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum FromFormat {
    Zola,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { ctx } => cmd_serve(ctx),
        Cmd::Build { ctx, output_dir } => cmd_build(ctx, output_dir),
        Cmd::Convert {
            ctx,
            from,
            overwrite,
            verify,
            output_dir,
            ground_truth,
            only,
            above,
            below,
            all_diffs,
        } => cmd_convert(
            ctx,
            from,
            overwrite,
            verify,
            output_dir,
            ground_truth,
            only,
            above,
            below,
            all_diffs,
        ),
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
            // `e` is already a self-describing `error: …` block (typst's rich
            // diagnostics for compile failures); no redundant prefix.
            eprintln!("{e}");
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
    match import_md(&src, "page", None) {
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

/// `twyla convert` — port a zola markdown site and validate it against the
/// ground truth. Drafts the missing `.typ`, compiles the whole site, diffs
/// every page, and runs the link audit; prints the finding list and a
/// `RESULT:` line. Exit 0 = clean, 1 = failures, 2 = setup/IO/compile error.
#[allow(clippy::too_many_arguments)]
fn cmd_convert(
    args: ContextArgs,
    from: FromFormat,
    overwrite: bool,
    verify: bool,
    output_dir: Option<PathBuf>,
    ground_truth: Option<PathBuf>,
    only: Option<String>,
    above: usize,
    below: usize,
    all_diffs: bool,
) -> ExitCode {
    let FromFormat::Zola = from; // only zola today
    let ctx = match resolve_ctx(args) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if ctx.base_url.is_none() {
        eprintln!(
            "convert requires --base-url (or TWYLA_BASE_URL) for the link \
             audit and anchor-link rewrite"
        );
        return ExitCode::from(2);
    }

    let mode = if verify {
        ConvertMode::Verify
    } else if overwrite {
        ConvertMode::Overwrite
    } else {
        ConvertMode::Generate
    };
    let ground_truth = ground_truth.unwrap_or_else(|| ctx.default_output_dir());
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();

    let opts = ConvertOptions {
        ctx,
        mode,
        ground_truth,
        output_dir,
        only,
        above,
        below,
        all_diffs,
    };
    match convert::run(opts) {
        Ok(findings) => {
            print!("{}", convert::report::render(&findings, color));
            if convert::report::has_failure(&findings) {
                ExitCode::from(1)
            } else {
                ExitCode::from(0)
            }
        }
        Err(e) => {
            if !e.findings.is_empty() {
                print!("{}", convert::report::render(&e.findings, color));
            }
            eprintln!("{}", e.message);
            ExitCode::from(2)
        }
    }
}

//! Terminal progress display for one compile.
//!
//! A compile is a sequence of phases, not one countable job: the set of pages
//! and assets is only discovered while the fixed-point loop runs, and the
//! number of layout passes isn't known up front. So instead of one overall
//! bar, each phase gets its own bar on the same line, one after another:
//!
//! ```text
//! eval        ████████████████████████  42/42
//! layout 1    ⠋
//! assets 1    ██████████▌░░░░░░░░░░░░░   7/17
//! layout 2    ⠋
//! ```
//!
//! Layout passes are spinners: a pass is one opaque `Bundle::create` call,
//! with no per-document progress to count.
//!
//! This is plain indicatif: the compile takes a [`MultiProgress`] and each
//! phase holds the [`ProgressBar`] that [`phase`] adds to it. Dropping a bar
//! clears it (indicatif's default `ProgressFinish::AndClear`) and removes it
//! from the `MultiProgress`, so a phase ends by dropping its bar — explicitly,
//! before the next phase starts, or on an early `?` return.

use std::borrow::Cow;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// The stderr line progress is drawn on (hidden by indicatif when stderr
/// isn't a terminal). A long-running caller (serve) makes one and shares it
/// between every compile and anything else that prints to stderr meanwhile,
/// which must print through [`MultiProgress::suspend`] so it doesn't land on
/// top of the line.
pub fn stderr() -> MultiProgress {
    MultiProgress::with_draw_target(ProgressDrawTarget::stderr())
}

/// A `MultiProgress` that never draws (tests, embedders).
pub fn hidden() -> MultiProgress {
    MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
}

/// Start a phase on `multi`: a counted bar of `total` steps (tick it with
/// [`ProgressBar::inc`]), or a spinner for `None`. Drop it to end the phase.
///
/// When `multi` is hidden this is a bare hidden bar, so redirected output
/// pays for no ticker thread.
pub fn phase(
    multi: &MultiProgress,
    label: impl Into<Cow<'static, str>>,
    total: Option<usize>,
) -> ProgressBar {
    if multi.is_hidden() {
        return ProgressBar::hidden();
    }
    // Fully configured before it's added: a bar draws on most setters, and
    // drawing half-configured would flash a stale label or count.
    let bar = ProgressBar::with_draw_target(total.map(|t| t as u64), ProgressDrawTarget::hidden());
    bar.set_style(match total {
        Some(_) => bar_style(),
        None => spinner_style(),
    });
    bar.set_message(label);
    let bar = multi.add(bar);
    // Keep the spinner (and elapsed time) moving while a step is slow.
    bar.enable_steady_tick(Duration::from_millis(80));
    bar
}

// Both styles share the leading columns (spinner, label), so a phase switch
// only changes what follows them:
//
//   ⠹ assets 1  ███████████████▋░░░░░░░░░░░░░░░░  139/220  12s
//   ⠹ layout 2  3s
fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.cyan} {msg:<9.bold} {bar:32.cyan/dim} {pos:>4}/{len:<4} {elapsed:.dim}",
    )
    .unwrap()
    .tick_chars(TICKS)
    // Solid blocks fill a shaded track, with eighth-blocks for a smooth edge.
    .progress_chars("█▉▊▋▌▍▎▏░")
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {msg:<9.bold} {elapsed:.dim}")
        .unwrap()
        .tick_chars(TICKS)
}

/// Braille spinner frames; the last entry is the "finished" frame.
const TICKS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ";

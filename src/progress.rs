//! Terminal progress display for one compile.
//!
//! A compile is a sequence of phases, not one countable job: the set of pages
//! and assets is only discovered while the fixed-point loop runs, and the
//! number of layout passes isn't known up front. So instead of one overall
//! bar, each phase gets its own line, replaced in place:
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
//! Drawn to stderr only when it's a terminal (indicatif hides the bar
//! otherwise); [`Progress::hidden`] never draws. The line is cleared when the
//! [`Progress`] is dropped, so an early `?` return leaves no residue.
//!
//! Each phase is a fresh bar, fully configured off-screen and then swapped
//! in. Reconfiguring one bar in place takes several setter calls, and
//! indicatif may draw between any two of them (e.g. the new count beside the
//! old label).

use std::borrow::Cow;
use std::sync::Mutex;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// The stderr line progress is drawn on. A long-running caller (serve) makes
/// one and shares it between every compile and anything else that prints to
/// stderr meanwhile, which must print through [`MultiProgress::suspend`] so it
/// doesn't land on top of the line. Hidden when stderr isn't a terminal.
pub fn stderr() -> MultiProgress {
    MultiProgress::with_draw_target(ProgressDrawTarget::stderr())
}

/// One compile's progress line. Shared across the threads typst parallelizes
/// over.
pub struct Progress {
    /// The stderr line phases are swapped into; `None` when hidden.
    multi: Option<MultiProgress>,
    /// The current phase's bar.
    bar: Mutex<ProgressBar>,
}

impl Progress {
    /// A progress line drawn on `target` (see [`stderr`]), or a no-op when
    /// there is none or it's hidden (stderr isn't a terminal), so redirected
    /// output pays for no ticker threads or locking.
    pub fn new(target: Option<&MultiProgress>) -> Self {
        match target {
            Some(multi) if !multi.is_hidden() => Self {
                multi: Some(multi.clone()),
                bar: Mutex::new(ProgressBar::hidden()),
            },
            _ => Self::hidden(),
        }
    }

    /// A progress handle that never draws (tests, embedders).
    pub fn hidden() -> Self {
        Self {
            multi: None,
            bar: Mutex::new(ProgressBar::hidden()),
        }
    }

    /// Start a new phase with its own label and count. `total: None` shows a
    /// spinner.
    pub fn phase(&self, label: impl Into<Cow<'static, str>>, total: Option<usize>) {
        let Some(multi) = &self.multi else { return };
        let next = ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden());
        match total {
            Some(total) => {
                next.set_style(bar_style());
                next.set_length(total as u64);
            }
            None => next.set_style(spinner_style()),
        }
        next.set_message(label);
        // Keep the spinner (and elapsed time) moving while a step is slow.
        next.enable_steady_tick(Duration::from_millis(80));
        let mut bar = self.bar.lock().unwrap();
        multi.remove(&bar);
        *bar = multi.add(next);
    }

    /// One step of the current phase done.
    pub fn tick(&self) {
        if self.multi.is_none() {
            return;
        }
        self.bar.lock().unwrap().inc(1);
    }

    /// Run `f` (which prints to the terminal) with the line hidden.
    pub fn suspend<R>(&self, f: impl FnOnce() -> R) -> R {
        match &self.multi {
            Some(multi) => multi.suspend(f),
            None => f(),
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(multi) = &self.multi {
            multi.remove(&self.bar.lock().unwrap());
            let _ = multi.clear();
        }
    }
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

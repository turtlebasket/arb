//! A sticky one-line status bar pinned to the bottom of the terminal.
//!
//! The watcher emits scrolling output through [`StatusLine::log`] (findings) and
//! [`StatusLine::note`] (diagnostics): the bar is erased, the message is printed
//! above it, then the bar is redrawn so it always sits on the last row. The bar
//! shows how many sealed blocks we've processed and appends an animated spinner
//! while an activity (init / scan / re-sync) is in flight — drive long awaits
//! through [`with_spinner`] so the spinner animates during the await itself.
//!
//! State is held behind `Cell`/`RefCell` so every method takes `&self`: the
//! spinner ticker and the in-flight future can both borrow the same `&StatusLine`
//! without aliasing. When stderr is not a TTY the bar is disabled and `log`/`note`
//! fall back to plain `println!`/`eprintln!`, preserving piped/redirected output.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::time::Duration;

/// Braille spinner frames (same set `cargo`/`spinners` use).
const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// `\r` to col 0 then `ESC[2K` to erase the whole line.
const CLEAR: &str = "\r\x1b[2K";

pub struct StatusLine {
    blocks: Cell<u64>,
    confirmed: Cell<u64>,
    activity: RefCell<Option<String>>,
    frame: Cell<usize>,
    drawn: Cell<bool>,
    enabled: bool,
}

impl StatusLine {
    pub fn new() -> Self {
        Self {
            blocks: Cell::new(0),
            confirmed: Cell::new(0),
            activity: RefCell::new(None),
            frame: Cell::new(0),
            drawn: Cell::new(false),
            enabled: std::io::stderr().is_terminal(),
        }
    }

    /// Set the processed-block count and redraw the bar.
    pub fn set_blocks(&self, n: u64) {
        self.blocks.set(n);
        self.redraw();
    }

    /// Set the confirmed-arb count and redraw the bar.
    pub fn set_confirmed(&self, n: u64) {
        self.confirmed.set(n);
        self.redraw();
    }

    /// Begin an activity (spinner shown). Pair with [`idle`](Self::idle) — or use
    /// [`with_spinner`], which brackets this for you.
    pub fn busy(&self, what: &str) {
        *self.activity.borrow_mut() = Some(what.to_string());
        self.redraw();
    }

    /// End the current activity (spinner hidden).
    pub fn idle(&self) {
        *self.activity.borrow_mut() = None;
        self.redraw();
    }

    /// Advance the spinner one frame. No-op while idle or disabled.
    pub fn tick(&self) {
        if self.enabled && self.activity.borrow().is_some() {
            self.frame.set(self.frame.get().wrapping_add(1));
            self.redraw();
        }
    }

    /// Emit a findings line above the bar (stdout when disabled).
    pub fn log(&self, msg: &str) {
        if !self.enabled {
            println!("{msg}");
            return;
        }
        self.emit(msg);
    }

    /// Emit a diagnostic line above the bar (stderr when disabled).
    pub fn note(&self, msg: &str) {
        if !self.enabled {
            eprintln!("{msg}");
            return;
        }
        self.emit(msg);
    }

    /// Erase the bar (call before printing the final summary).
    pub fn finish(&self) {
        if self.enabled && self.drawn.get() {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "{CLEAR}");
            let _ = err.flush();
            self.drawn.set(false);
        }
    }

    /// Clear the bar, print `msg` + newline (scrolls), then redraw the bar. All on
    /// stderr (where the bar lives) so cursor bookkeeping stays on one stream.
    fn emit(&self, msg: &str) {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{CLEAR}{msg}");
        let _ = err.flush();
        drop(err);
        self.drawn.set(false);
        self.redraw();
    }

    fn redraw(&self) {
        if !self.enabled {
            return;
        }
        // Dim so the bar reads as chrome, not content.
        let mut bar = format!("\x1b[2mblocks {}", self.blocks.get());
        let c = self.confirmed.get();
        if c > 0 {
            bar.push_str(&format!(" | arbs {c}"));
        }
        if let Some(a) = self.activity.borrow().as_ref() {
            let f = FRAMES[self.frame.get() % FRAMES.len()];
            // Spinner un-dimmed so the motion is visible.
            bar.push_str(&format!("\x1b[0m  {f} {a}…\x1b[2m"));
        }
        bar.push_str("\x1b[0m");
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "{CLEAR}{bar}");
        let _ = err.flush();
        self.drawn.set(true);
    }
}

impl Default for StatusLine {
    fn default() -> Self {
        Self::new()
    }
}

/// Run `fut` to completion while animating the bar's spinner under `what`. The
/// spinner is driven by a 100ms timer raced against `fut`, so it keeps moving
/// during the await (unlike a plain `busy`/`idle` bracket, which would freeze for
/// the duration of a single blocking `.await`). `fut` may borrow `&status` too
/// (e.g. to `log` findings) — both hold shared borrows, no aliasing.
pub async fn with_spinner<F: Future>(status: &StatusLine, what: &str, fut: F) -> F::Output {
    if !status.enabled {
        return fut.await;
    }
    status.busy(what);
    tokio::pin!(fut);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.tick().await; // consume immediate first tick
    let out = loop {
        tokio::select! {
            o = &mut fut => break o,
            _ = tick.tick() => status.tick(),
        }
    };
    status.idle();
    out
}

//! Non-interactive frontend smoke tests for platform validation.
//!
//! Driven by environment variables rather than CLI flags to keep CI machinery internal.
//! A dedicated watchdog thread ensures hangs fail promptly with diagnostic output.

use std::time::Duration;

use tracing::{error, info};

use crate::App;

/// Presence of a non-empty value is what turns smoke mode on.
const MARKER_ENV: &str = "FELIS_SMOKE_MARKER";

/// Milliseconds; default [`DEFAULT_TIMEOUT_MS`].
const TIMEOUT_ENV: &str = "FELIS_SMOKE_TIMEOUT_MS";

/// A cold Windows runner pays for adapter enumeration, shader
/// compilation, font discovery, and a daemon autospawn before the first
/// frame exists.
const DEFAULT_TIMEOUT_MS: u64 = 90_000;

/// A cursor block alone clears this by an order of magnitude; the floor
/// only rejects a frame whose entire content is a stray speck.
const MIN_PAINTED_PIXELS: u64 = 32;

/// Distinct from the 101 a panic yields and the 1 an `anyhow` error
/// bubbles up as, so a CI log says which of the three happened.
const EXIT_NO_EVIDENCE: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Window and renderer exist; the marker has not been typed yet.
    AwaitFirstFrame,
    /// The marker was typed; waiting for the daemon's echo to reach
    /// the shadow screen.
    AwaitEcho,
    /// Verdict reached; the loop is unwinding.
    Done,
}

pub(crate) struct Smoke {
    marker: String,
    timeout: Duration,
    stage: Stage,
    watchdog_armed: bool,
}

impl Smoke {
    /// `None` leaves the client entirely unaffected.
    pub(crate) fn from_env() -> Option<Self> {
        let marker = std::env::var(MARKER_ENV).ok().filter(|m| !m.is_empty())?;
        let timeout = std::env::var(TIMEOUT_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        info!(
            marker,
            timeout_ms = timeout,
            "frontend smoke armed; the window will drive itself and exit"
        );
        Some(Self {
            marker,
            timeout: Duration::from_millis(timeout),
            stage: Stage::AwaitFirstFrame,
            watchdog_armed: false,
        })
    }

    /// A loop that unwound without a verdict leaves the process obliged to
    /// exit nonzero.
    pub(crate) const fn reached_verdict(&self) -> bool {
        matches!(self.stage, Stage::Done)
    }

    pub(crate) fn arm_watchdog(&mut self) {
        if self.watchdog_armed {
            return;
        }
        self.watchdog_armed = true;
        Self::exit_on_panic();
        let budget = self.timeout;
        std::thread::spawn(move || {
            std::thread::sleep(budget);
            error!(
                timeout_ms = budget.as_millis(),
                "frontend smoke timed out before a verified frame"
            );
            // Not `event_loop.exit()`: the watchdog exists for the case
            // where the loop itself stopped making progress.
            std::process::exit(EXIT_NO_EVIDENCE);
        });
    }

    /// Windows runs its error reporting on an unhandled fault, and that
    /// suspends every thread while it collects a dump, the watchdog
    /// included, so a wgpu validation panic would hang the gate past the
    /// job's whole budget. Exiting from the hook keeps the panic message
    /// (the default hook runs first) and never reaches the fault path.
    fn exit_on_panic() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            previous(info);
            std::process::exit(EXIT_NO_EVIDENCE);
        }));
    }
}

impl App {
    /// Called once per painted frame: every stage's precondition is
    /// something a frame either shows or does not.
    pub(crate) fn smoke_step(&mut self) {
        let Some(stage) = self.smoke.as_ref().map(|smoke| smoke.stage) else {
            return;
        };
        match stage {
            Stage::AwaitFirstFrame => {
                let Some(marker) = self.smoke.as_ref().map(|smoke| smoke.marker.clone()) else {
                    return;
                };
                // No trailing newline: `felis-smoke-abc: command not found`
                // would also contain the marker while proving less.
                self.ferry_key_bytes(marker.into_bytes());
                if let Some(smoke) = self.smoke.as_mut() {
                    smoke.stage = Stage::AwaitEcho;
                }
            }
            Stage::AwaitEcho => {
                if self.smoke_marker_on_screen() {
                    self.smoke_verdict();
                }
            }
            Stage::Done => {}
        }
    }

    /// The proof that input reached the PTY and grid frames came back.
    fn smoke_marker_on_screen(&self) -> bool {
        let Some(marker) = self.smoke.as_ref().map(|smoke| smoke.marker.as_str()) else {
            return false;
        };
        let screen = self.shadow.screen();
        let clusters = screen.cluster_table();
        (0..screen.rows()).any(|row| {
            screen
                .row_content(row)
                .is_some_and(|cells| felis_grid::row_text_trim(cells, clusters).contains(marker))
        })
    }

    /// A pass detaches like any other clean exit (the session survives);
    /// anything else leaves the process with a nonzero status.
    fn smoke_verdict(&mut self) {
        let capture = self
            .surface
            .as_ref()
            .and_then(|surface| surface.renderer.capture_last_frame());
        let Some(capture) = capture else {
            error!("frontend smoke: the frame could not be read back from the GPU");
            std::process::exit(EXIT_NO_EVIDENCE);
        };
        if capture.painted_pixels < MIN_PAINTED_PIXELS || capture.distinct_colors < 2 {
            error!(
                width = capture.width,
                height = capture.height,
                painted_pixels = capture.painted_pixels,
                distinct_colors = capture.distinct_colors,
                "frontend smoke: the marker is on screen but the frame is blank"
            );
            std::process::exit(EXIT_NO_EVIDENCE);
        }
        info!(
            width = capture.width,
            height = capture.height,
            total_pixels = capture.total_pixels,
            painted_pixels = capture.painted_pixels,
            distinct_colors = capture.distinct_colors,
            "frontend smoke passed: window, surface, input, echo, and a painted frame"
        );
        if let Some(smoke) = self.smoke.as_mut() {
            smoke.stage = Stage::Done;
        }
        // Exits by way of `AppEvent::Detached`, so the session it created
        // stays in the pool like after an ordinary window close.
        self.request_detach("frontend smoke passed");
    }
}

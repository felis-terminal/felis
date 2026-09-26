//! `felis-client`: the native GUI client. Renders the daemon's grid via
//! wgpu; closing the window detaches, and the daemon's session lives on.

// `deny`, not `forbid`: the two sanctioned `allow(unsafe_code)` sites
// (`macos_window`, `hyperlink`'s `ShellExecuteW`) would be refused by `forbid`.
#![cfg_attr(not(test), deny(unsafe_code))]

use std::{
    num::NonZeroU8,
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
pub(crate) use felis_client_core::local_socket::resolve_local_socket;
use felis_client_core::{
    ActivationTarget, Carrier, Clipboard, ConfigSource, DialError, DialedConnection, GridPos,
    ImageId, ImageShadow, InMemoryClipboard, Keymap, LANDING_REMOTE_SPAWN, Landing, Offer,
    Reconnector, Selection, SelectionMode, ShadowScreen,
    action::{Action, ClipboardScope, IpcAction, SwitchDirection},
    config,
    config::{Backdrop, BuiltinShader, GUI_CLIENT_ID, PostShaderChoice, compose_window_title},
    config_watcher, cursor_blink, cursor_trail, dial_and_land, dial_and_land_within,
    doctor::{ClipboardProbe, GpuProbe, PROBE_VERSION, ProbeReport},
    launch_args,
    outgoing::{OutgoingFrame, OutgoingFull, OutgoingQueue},
    pipe::StagedRegion,
    pull, reconnector_for_target, redraw,
    roster::{RingKey, pick_switch_target},
    session_id::validate_session_id_prefix,
    shader_clock,
    viewport::{composed_row_for_line, viewport_for_hit},
};
use felis_client_core::{CarrierReader, CarrierWriter};
use felis_transport::logging::{self, Console};

fn build_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")
}
use felis_protocol::{
    MessageKind, SessionHex, codec,
    messages::{
        ColSpan, ConnToClientMsg, ConnToDaemonMsg, Correlation, GridMsg, ImageMsg, InputMsg,
        MouseAction, MouseButton, MouseEvent, OpsToClientMsg, PushMsg, RegionToClientMsg,
        RegionToDaemonMsg, RetargetTarget, SearchToClientMsg, SearchToDaemonMsg, SessionInfo,
        SessionToDaemonMsg, StreamId, Subject,
    },
};
use felis_render_wgpu::{
    DEFAULT_FONT_SIZE_LOGICAL_PX, FaceSpec, ImageData, ImageSource, MouseState, PreeditOverlay,
    Renderer, RendererConfig, SearchHitSpan, SearchOverlay, SurfaceError, TRAIL_SHADER_SOURCE,
    TrailState, Warmup, validate_post_shader,
};
use felis_transport::{ClientDriver, Delivery, FrameReader, FrameWriter, Incoming, OwnedFrame};
use std::sync::{Mutex, PoisonError};
use tracing::{debug, error, info, warn};
use winit::{
    application::ApplicationHandler,
    dpi::{PhysicalPosition, PhysicalSize},
    event::{
        ElementState, Ime, KeyEvent, MouseButton as WinitMouseButton, MouseScrollDelta, WindowEvent,
    },
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{UserAttentionType, Window, WindowAttributes, WindowId},
};

mod app_methods;
mod event_handler;
mod exit_ladder;
mod hyperlink;
mod input;
mod keymap_adapter;
#[cfg(target_os = "macos")]
mod macos_window;
mod mem;
mod navigation;
mod os_clipboard;
mod smoke;
mod switch_intent;
mod winit_keys;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use hyperlink::{
    activation_matches_preview, activation_target, grid_msg_moves_hover_target, open_url,
    pointer_icon,
};

/// User events posted from the IPC reader task to the winit loop.
///
/// Connection-scoped variants carry a `conn_gen` stamp to drop stale frames
/// across session switches; window-scoped variants carry no stamp to avoid
/// losing events that arrived just before a switch.
enum AppEvent {
    GridFrame {
        msg: GridMsg,
        conn_gen: u64,
    },
    ImageFrame {
        msg: ImageMsg,
        conn_gen: u64,
    },
    SearchFrame {
        msg: SearchToClientMsg,
        conn_gen: u64,
    },
    /// Carries the stream id so a terminal for the stream Esc just
    /// canceled cannot revive a bar the user already dismissed.
    SearchEnded {
        stream_id: StreamId,
        /// `Err(reason)` is the daemon's typed refusal (empty needle, regex
        /// compile failure, the session going away under the walk).
        outcome: Result<u32, String>,
        conn_gen: u64,
    },
    /// A superseded connection's EOF is the ordinary end of a switch's
    /// detach and says nothing about the window's current daemon.
    DaemonClosed {
        conn_gen: u64,
    },
    /// The session lives on in the daemon pool: a `detach`-bound chord, or
    /// `felis sessions evict` (`PushMsg::Evicted`,
    /// docs/explanation/architecture/control-surfaces.md). Distinct from
    /// [`Self::DaemonClosed`] so the pipe-return path does not misfire.
    Detached,
    /// Only posted when `FELIS_STARTUP_EXIT_MS` is set: logs the warmed-up
    /// RSS (`felis::mem` `before-exit`) and exits cleanly, which also lets
    /// the `dhat-heap` profiler's `Drop` write `dhat-heap.json`.
    StartupExit,
    /// `config.toml` changed on disk (posted by `config_watcher`); runs the
    /// same reload path as Ctrl+Shift+R.
    ConfigReloaded,
    /// A fresh daemon connection finished its handshake and attached. The
    /// handler builds the new [`Pumps`] itself: building it in the task
    /// would race the reader against this event on the proxy queue.
    SessionSwitchReady {
        reader: FrameReader<CarrierReader>,
        writer: FrameWriter<CarrierWriter>,
        /// Already past both handshakes; a driver rebuilt on this side
        /// would know nothing of the ids the dial's round trips issued.
        driver: SharedDriver,
        attach: Box<SessionInfo>,
        /// Per-connection because a retarget can cross carriers (local
        /// pulls, SSH does not).
        pull_enabled: bool,
        /// The descriptor the window is now on after a cross-carrier
        /// retarget (`window retarget`); `None` for a same-daemon switch.
        retargeted: Option<Reconnector>,
        record: TrailRecord,
    },
    /// The reconnect ladder is spent or the daemon stated a verdict a
    /// later attempt cannot change. `None` is the one verdict that is
    /// not terminal: the session ended, so the window takes the exit
    /// ladder instead of closing.
    ReconnectFailed {
        reason: Option<ExitReason>,
        detail: String,
    },
    /// The App stays attached to its current session and clears the
    /// in-progress guard so a follow-up chord can retry.
    SessionSwitchFailed {
        reason: String,
        /// The one failure class B-11 answers by re-picking rather than
        /// giving up (`DialError::target_vanished`).
        target_vanished: bool,
    },
    /// The daemon answered the `Ops::List` a switch chord issued; there is
    /// no cached roster.
    RosterListed {
        /// Only the listing that answers the outstanding fetch may move the
        /// window (A-5/A-7).
        request: felis_protocol::messages::RequestId,
        sessions: Vec<SessionInfo>,
        conn_gen: u64,
    },
    /// A daemon that streams the grid but never answers `Ops::List` costs
    /// one chord rather than every chord for the rest of the connection.
    RosterFetchTimedOut {
        request: felis_protocol::messages::RequestId,
        conn_gen: u64,
    },
    /// Daemon answered a `RegionToDaemonMsg::Request`
    /// (docs/explanation/input.md "Which daemon runs the command"); the
    /// App feeds it to the sink the chord named, on this machine.
    PipeRegionReady {
        data: Vec<u8>,
        /// For the child's `FELIS_*` position variables and the built-in
        /// pager's `+N`.
        position: Option<felis_protocol::messages::RegionPosition>,
    },
    /// `Conn::Error { subject: Request(_) }`. The connection lives on
    /// (A-5), so the handler releases whatever that request had parked;
    /// the id tells the pipe sink from the roster fetch, since a window
    /// has at most one of each outstanding.
    RequestRefused {
        request: felis_protocol::messages::RequestId,
        detail: String,
        conn_gen: u64,
    },
    /// `PushMsg::Reattach` from `felis sessions switch`
    /// (docs/explanation/architecture/control-surfaces.md). Runs the same
    /// switch path a chord would, so the push cannot put the window
    /// anywhere a keypress couldn't.
    ReattachRequested {
        /// The observing pump's, not the window's: a cross-carrier
        /// landing handled first would otherwise attach this id in a
        /// namespace it was never issued in.
        reconnector: Reconnector,
        id: u128,
    },
    /// `PushMsg::RetargetHost` from `window retarget`
    /// (docs/explanation/architecture/control-surfaces.md). Re-dials the
    /// carrier like a switch does, so the push cannot put the window
    /// anywhere a local invocation couldn't.
    RetargetRequested {
        target: RetargetTarget,
    },
    /// `PushMsg::SessionExited` (PTY EOF). Once the queued rows are
    /// applied the window takes the exit ladder, and closes only when
    /// every rung of it is spent. Distinct from [`Self::Detached`]: the
    /// session does not survive here.
    SessionExited {
        conn_gen: u64,
        /// The observing pump's reconnector with the exited id: an id
        /// alone names nothing across daemons.
        place: exit_ladder::Place,
    },
}

/// What a landing does to the trail
/// (`architecture/session-lifecycle.md`). Separate from
/// [`SwitchState::InFlight`]'s `session_gone`, which answers a different
/// question: whether there is a live session to stay on if this landing
/// fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrailRecord {
    /// The transport reconnect, which lands on the place it never left:
    /// nothing about the trail or a pipe/run visit in progress changes.
    Reconnect,
    /// An exit-driven landing on a real place: pushes nothing, and any
    /// pipe/run visit ends here.
    Keep,
    Push,
    /// The switch into a pipe/run transient: the transient itself is
    /// never a place to come back to, and the window sits on one until
    /// it lands somewhere else.
    PushIntoTransient,
    /// Landing requested by an intent during a visit. Pushes nothing and
    /// preserves the parked handoff for the unwind viewport. The target
    /// session is treated as an ordinary session rather than transient so
    /// subsequent moves out of it are recorded on the trail.
    CarryHandoff,
}

/// Frontend-launch subcommands. `felis-client` is the GUI frontend the
/// `felis` front-door (felis-cli) execs; the headless IPC verbs live
/// there, not here. `None` (no subcommand) keeps the bare-launch
/// "create a new session" semantics.
#[derive(Debug, Subcommand)]
enum ClientCmd {
    /// Attach a window to an existing session (resolved as a unique hex
    /// prefix) instead of creating one. `felis attach <id>` reaches here
    /// via the front-door; `felis-client attach <id>` works directly too
    /// (e.g. the macOS .app).
    Attach {
        /// Hex id or any unique prefix.
        #[arg(value_name = "ID-OR-PREFIX", value_parser = validate_session_id_prefix)]
        id: String,
    },
}

/// The canonical identity line (`docs/reference/workspace.md`
/// "Versioning"): the revision (from build.rs) answers "which build is
/// this?" where the semver cannot. Printed in full rather than
/// abbreviated because `felis version` parses this line back into a
/// `BuildIdentity`.
const FELIS_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("FELIS_BUILD_STAMP"),
    ")"
);

/// `felis-client`: native GUI frontend CLI surface. The `felis`
/// front-door owns the user-facing help / completions / man pages and
/// the headless verbs; this is the narrower surface it execs with, also
/// usable directly.
#[derive(Debug, Parser)]
#[command(name = "felis-client", version = FELIS_VERSION)]
struct Cli {
    /// Subcommand. `attach <id>` reattaches to an existing session;
    /// absent means create a new one.
    #[command(subcommand)]
    cmd: Option<ClientCmd>,
    /// Connect to a remote daemon over SSH stdio (`user@host`, or any
    /// destination `ssh` accepts). felis spawns
    /// `ssh <host> felis-daemon relay` and feeds framed bytes through.
    /// SSH is the auth boundary (docs/reference/ipc.md "Cross-host
    /// carrier: SSH stdio"); no felis-side credential is collected.
    #[arg(long, value_name = "user@host")]
    host: Option<String>,
    /// Extra argument passed verbatim to `ssh` before the destination
    /// (repeatable): `--ssh-arg=-p --ssh-arg=2222`, `--ssh-arg=-i
    /// --ssh-arg=~/.ssh/vm_key`. For an ad-hoc VM not worth an
    /// `~/.ssh/config` entry. felis does not interpret these, only
    /// `ssh` does. Requires `--host`.
    #[arg(long = "ssh-arg", value_name = "TOKEN", requires = "host")]
    ssh_arg: Vec<String>,
    /// Override the daemon socket path. Default:
    /// `/tmp/felis.<uid>/daemon.sock`, derived from the uid alone; a
    /// path given here must sit in a `0700` directory you own. Useful
    /// for independent daemons (work / home / test) under one user.
    /// Mutually exclusive with `--host` (its own carrier, SSH stdio).
    #[arg(long, value_name = "PATH", conflicts_with = "host")]
    socket: Option<PathBuf>,
    /// Read this `config.toml` instead of the platform default for startup
    /// and live reload. Always an absolute path: the `felis` front door
    /// resolves relative paths against the invocation directory, which
    /// this process cannot recover.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Program and args to run instead of `$SHELL` (`xterm -e` analogue).
    /// Arguments after `--` are taken literally as argv (`felis -- htop`).
    /// The session persists across window close and child exit until removed
    /// via `felis sessions kill <id>`. Ignored when `attach` is given.
    #[arg(last = true, value_name = "CMD")]
    command: Vec<String>,
    /// Widen the log filter so the client's hot-path perf trace
    /// events fire (redraw-burst counters, IME / selection
    /// debug spans). Overrides the default `info,felis=debug`
    /// filter only when `RUST_LOG` is unset.
    #[arg(long)]
    trace_perf: bool,
    /// Report GPU and OS-clipboard availability as JSON on stdout, then
    /// exit without opening a window or dialing a daemon.
    ///
    /// Hidden handoff for `felis doctor` to probe wgpu and arboard without
    /// linking them into the CLI binary directly.
    #[arg(long, hide = true, exclusive = true)]
    doctor_probe: bool,
}

const fn log_filter_directive(trace_perf: bool) -> &'static str {
    if trace_perf {
        "info,felis=debug,felis::redraw=trace,felis::pull=trace,felis::main=trace"
    } else {
        "info,felis=debug"
    }
}

fn main() -> Result<ExitCode> {
    // Bound first so it brackets every later allocation.
    #[cfg(feature = "dhat-heap")]
    let _dhat = dhat::Profiler::new_heap();

    let cli = Cli::parse();
    if cli.doctor_probe {
        // Before `logging::init`: the console sink is stdout, and a log
        // line on the channel carrying the JSON would corrupt the report.
        return run_doctor_probe().map(|()| ExitCode::SUCCESS);
    }
    // Tee logs into a per-user file: a `.app` launch inherits `/dev/null`
    // for stdout/stderr, so console-only logging records nothing when a
    // window dies in the field.
    logging::init(
        Console::Stdout,
        Some("client.log"),
        log_filter_directive(cli.trace_perf),
    );

    let attach_target: Option<String> = cli.cmd.map(|ClientCmd::Attach { id }| id);

    mem::log_rss("startup");

    let config_source = cli
        .config
        .clone()
        .map_or(ConfigSource::Default, ConfigSource::Explicit);
    let cfg = config::EffectiveConfig::load_from_source(&config_source, GUI_CLIENT_ID);
    let compiled_keymap = cfg.keymap.compile(cfg.source_dir.as_deref());
    info!(
        font_family = ?cfg.font.family,
        font_size = ?cfg.font.size_px,
        theme_fg = ?cfg.theme.foreground,
        theme_bg = ?cfg.theme.background,
        clipboard_use_os = cfg.clipboard.use_os_clipboard,
        clipboard_osc_52 = ?cfg.clipboard.osc_52,
        keymap_entries = compiled_keymap.len(),
        source = ?config_source.path(),
        "config loaded"
    );
    let startup = ClientStartup {
        // Before the dial: on a cold start the GPU work hides the daemon
        // spawn, and it needs neither the window nor the session.
        warmup: Warmup::start_gpu(),
        renderer_cfg: renderer_config_from(&cfg),
        font_size_logical_px: configured_font_size_logical_px(&cfg),
        clipboard_cfg: cfg.clipboard,
        window_cfg: cfg.window,
        cursor_cfg: cfg.cursor,
        shader_cfg: cfg.shader.clone(),
        mouse_cfg: cfg.mouse,
        keymap: Keymap::default_for_platform().with_overrides(compiled_keymap),
        config_source,
    };

    let runtime = build_runtime()?;
    let _enter = runtime.enter();

    // The Reconnector doubles as the launch descriptor so the initial
    // handshake and every re-attach negotiate identically: the daemon
    // never sees a leaner subset appear mid-window.
    let reconnector = match cli.host {
        None => Reconnector {
            carrier: Carrier::Local(resolve_local_socket(cli.socket.as_deref())?.into()),
            offer: Offer::window(true),
        },
        Some(host) => {
            info!(host = %host, "connecting via ssh stdio");
            Reconnector {
                carrier: Carrier::Ssh {
                    destination: host,
                    ssh_args: cli.ssh_arg,
                },
                offer: Offer::window(false),
            }
        }
    };

    let mode = match attach_target {
        Some(id) => AttachMode::Attach(id),
        None => AttachMode::Create(launch_args(cli.command, &reconnector.carrier)?),
    };
    run_client(&runtime, startup, mode, reconnector)
}

/// Probe GPU and clipboard unconditionally regardless of user config, so
/// disabling clipboard does not report hardware unavailability. Neither
/// check is fatal: failures produce structured reports for the caller to
/// merge instead of guessing from exit codes.
fn run_doctor_probe() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    let gpu = runtime
        .block_on(felis_render_wgpu::probe::probe_adapter())
        .map_or_else(GpuProbe::unavailable, |found| GpuProbe {
            available: true,
            name: Some(found.name),
            backend: Some(found.backend),
            device_type: Some(found.device_type),
            driver: Some(found.driver),
        });
    let clipboard = match os_clipboard::OsClipboard::new(false) {
        Ok(_) => ClipboardProbe {
            available: true,
            detail: None,
        },
        Err(err) => ClipboardProbe {
            available: false,
            detail: Some(err.to_string()),
        },
    };
    let report = ProbeReport {
        v: PROBE_VERSION,
        client_version: FELIS_VERSION.to_owned(),
        gpu,
        clipboard,
    };
    let text = serde_json::to_string(&report).context("serialize probe report")?;
    #[expect(
        clippy::print_stdout,
        reason = "stdout is this mode's whole output channel"
    )]
    {
        println!("{text}");
    }
    Ok(())
}

/// Session listing lives on felis-cli's `cli_sessions::cmd_list` and
/// never reaches this enum.
#[derive(Debug, Clone)]
enum AttachMode {
    /// An empty `command` lets the daemon's factory pick `$SHELL`.
    Create(felis_protocol::messages::SpawnArgs),
    /// Hex prefix (validated by `validate_session_id_prefix`), resolved
    /// against the daemon's session list at runtime.
    Attach(String),
}

/// Falls back to the in-process bag when arboard can't reach a surface
/// (no display, sandbox, headless CI) so Ctrl+Shift+V still works for
/// felis-internal round-trips.
fn build_clipboard(cfg: &config::ClipboardConfig) -> Arc<dyn Clipboard> {
    if !cfg.use_os_clipboard {
        info!(
            "clipboard.use_os_clipboard = false; using in-process bag (clipboard.osc_52 is inert without an OS surface)"
        );
        return Arc::new(InMemoryClipboard::new());
    }
    match os_clipboard::OsClipboard::new(cfg.osc_52.writes_to_system()) {
        Ok(os) => Arc::new(os),
        Err(err) => {
            warn!(
                ?err,
                "OS clipboard unavailable; falling back to in-process bag"
            );
            Arc::new(InMemoryClipboard::new())
        }
    }
}

#[cfg(target_os = "linux")]
const APP_ID: &str = "felis";

/// On macOS, `decorations == false` retains the `Titled` mask with a hidden,
/// transparent titlebar instead of winit's `Borderless` mode. This keeps
/// OS-rounded corners and drop shadows while the full-size content view
/// sits in front to receive terminal clicks.
#[must_use]
fn window_attributes(title: String, decorations: bool, transparent: bool) -> WindowAttributes {
    // `with_transparent(true)` only when `window.opacity < 1.0`: an
    // always-on transparent surface costs an extra compositor blend and,
    // on some WMs, drops the opaque-window optimization.
    let attrs = Window::default_attributes()
        .with_title(title)
        .with_transparent(transparent)
        .with_inner_size(PhysicalSize::new(960u32, 600u32));

    // Wayland's app_id and X11's WM_CLASS share one winit field, so the
    // Wayland setter covers both backends. It must match
    // share/applications/felis.desktop, or the WM cannot pair the window
    // with that entry.
    #[cfg(target_os = "linux")]
    let attrs = {
        use winit::platform::wayland::WindowAttributesExtWayland;
        attrs.with_name(APP_ID, APP_ID)
    };

    #[cfg(target_os = "macos")]
    if !decorations {
        use winit::platform::macos::WindowAttributesExtMacOS;
        // NOT `with_decorations(false)`, which would force Borderless.
        return attrs
            .with_titlebar_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_hidden(true)
            .with_titlebar_buttons_hidden(true);
    }

    attrs.with_decorations(decorations)
}

/// Applies the OS-native window backdrop (docs/reference/config.md).
/// macOS hosts an `NSVisualEffectView` when translucent; Windows sets DWM
/// system backdrops; Linux compositors blur independently. Each platform
/// branch clears prior effects first so reloads are idempotent.
fn apply_window_backdrop(window: &Window, backdrop: Backdrop, transparent: bool) {
    #[cfg(target_os = "macos")]
    {
        use window_vibrancy::{
            NSVisualEffectMaterial, NSVisualEffectState, apply_vibrancy, clear_vibrancy,
        };

        let blur = backdrop == Backdrop::Blur;
        if !blur || !transparent {
            if blur {
                warn!(
                    "window.backdrop = \"blur\" needs window.opacity < 1.0 to show through; leaving the window unblurred"
                );
            } else if backdrop != Backdrop::None {
                warn!(
                    ?backdrop,
                    "window.backdrop names a Windows material; on macOS use \"blur\""
                );
            }
            if let Err(err) = clear_vibrancy(window) {
                warn!(?err, "clear_vibrancy failed");
            }
            return;
        }
        // `UnderWindowBackground` is neutral frosted material; `Active`
        // keeps blur when key focus is lost. Corner radius is `None`
        // because `macos_window` re-clips the root layer to the window's
        // rounded shape.
        if let Err(err) = apply_vibrancy(
            window,
            NSVisualEffectMaterial::UnderWindowBackground,
            Some(NSVisualEffectState::Active),
            None,
        ) {
            warn!(?err, "apply_vibrancy failed; window stays unblurred");
        }
    }
    #[cfg(target_os = "windows")]
    {
        use window_vibrancy::{
            apply_acrylic, apply_mica, apply_tabbed, clear_acrylic, clear_mica, clear_tabbed,
        };

        let _ = transparent;
        // The three backdrops are mutually exclusive DWM attributes; clear
        // the others so a live reload replaces rather than layers.
        if backdrop != Backdrop::Acrylic {
            drop(clear_acrylic(window));
        }
        if backdrop != Backdrop::Mica {
            drop(clear_mica(window));
        }
        if backdrop != Backdrop::Tabbed {
            drop(clear_tabbed(window));
        }
        // Set regardless of body opacity: it styles the non-client title
        // bar even over an opaque body (the `mica` + `opacity = 1.0`
        // glass-titlebar combo). `None` follows the system light/dark.
        let res = match backdrop {
            Backdrop::None => Ok(()),
            Backdrop::Blur => {
                warn!(
                    "window.backdrop = \"blur\" is the macOS material; on Windows use \"acrylic\", \"mica\", or \"tabbed\""
                );
                Ok(())
            }
            Backdrop::Acrylic => apply_acrylic(window, None),
            Backdrop::Mica => apply_mica(window, None),
            Backdrop::Tabbed => apply_tabbed(window, None),
        };
        if let Err(err) = res {
            warn!(
                ?err,
                ?backdrop,
                "applying the Windows system backdrop failed; window stays plain"
            );
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (window, transparent);
        if backdrop != Backdrop::None {
            warn!(
                ?backdrop,
                "window.backdrop is honored only on the platform owning that material; on this platform blur is the compositor's job — enable it in your WM"
            );
        }
    }
}

/// Drives the hot-reload path: a theme-only change skips the font rebuild.
/// The size is not here: `RendererConfig` carries the physical size,
/// filled in per window from the scale factor, so the configured logical
/// baseline is compared on its own in `reload_config`.
fn font_settings_differ(a: &RendererConfig, b: &RendererConfig) -> bool {
    a.font_family != b.font_family
        || a.font_fallbacks != b.font_fallbacks
        || a.font_features != b.font_features
        || styled_faces_differ(a, b)
}

/// A `[font.bold]` / `[font.italic]` / `[font.bold_italic]` change loads
/// different styled faces, so it needs the fontdb-rescan reload path.
fn styled_faces_differ(a: &RendererConfig, b: &RendererConfig) -> bool {
    a.font_bold != b.font_bold
        || a.font_italic != b.font_italic
        || a.font_bold_italic != b.font_bold_italic
}

/// The inputs to `FontStack::auto_discover` that need a fontdb rescan.
/// False with `font_settings_differ` true means size-only and/or
/// features-only: [`Renderer::reload_font_size`] /
/// [`Renderer::reload_font_features`].
fn font_stack_inputs_differ(a: &RendererConfig, b: &RendererConfig) -> bool {
    a.font_family != b.font_family
        || a.font_fallbacks != b.font_fallbacks
        || styled_faces_differ(a, b)
}

/// Features are shape-time, not fontdb-time, so a features-only edit
/// must NOT rebuild the `FontStack` (a fontdb rescan stutters the reload).
fn font_features_differ(a: &RendererConfig, b: &RendererConfig) -> bool {
    a.font_features != b.font_features
}

/// Gates the re-report on reload: the daemon answers `OSC 10/11/12 ; ?`
/// from the last frame the client sent. Narrower than "the theme
/// changed": the palette and background alpha are not in the report.
fn reported_theme_differs(a: &RendererConfig, b: &RendererConfig) -> bool {
    a.theme_fg != b.theme_fg || a.theme_bg != b.theme_bg || a.theme_cursor != b.theme_cursor
}

/// What the zoom chords start from and Ctrl+Shift+0 returns to; sanitized
/// so a NaN or a 200-px typo never reaches the shaper.
fn configured_font_size_logical_px(cfg: &config::EffectiveConfig) -> Option<f32> {
    cfg.font.sanitized_font_size_logical_px()
}

/// The font size is left `None`: it is physical pixels, and the scale
/// factor only exists once the window does.
fn renderer_config_from(cfg: &config::EffectiveConfig) -> RendererConfig {
    RendererConfig {
        font_family: cfg.font.family.clone(),
        font_size_physical_px: None,
        theme_fg: cfg.theme.foreground.clone(),
        theme_bg: cfg.theme.background.clone(),
        theme_palette: cfg.theme.palette.clone().into_overrides(),
        font_fallbacks: cfg.font.fallback.iter().map(face_spec).collect(),
        font_features: cfg.font.features.clone(),
        font_bold: face_spec(&cfg.font.bold),
        font_italic: face_spec(&cfg.font.italic),
        font_bold_italic: face_spec(&cfg.font.bold_italic),
        theme_cursor: cfg.cursor.color.clone(),
        // Map fully-opaque to `None` so the renderer's opaque default
        // path stays byte-for-byte unchanged; only `opacity < 1.0`
        // carries an alpha and flips the surface to a transparent mode.
        background_opacity: {
            let a = cfg.window.clamped_opacity();
            (a < 1.0).then_some(a)
        },
        post_shader_wgsl: resolve_post_shader(cfg),
        font_files: Vec::new(),
    }
}

fn face_spec(c: &config::FontStyleConfig) -> FaceSpec {
    FaceSpec {
        family: c.family.clone(),
        features: c.features.clone(),
    }
}

/// A named shader that cannot be loaded or does not validate is refused
/// here, before the device sees it: felis renders normally instead of
/// blacking the window (docs/explanation/rendering/pipeline.md "Loading
/// and failure").
fn resolve_post_shader(cfg: &config::EffectiveConfig) -> Option<String> {
    match cfg.post_shader_choice() {
        PostShaderChoice::None => None,
        PostShaderChoice::Builtin(BuiltinShader::Trail) => Some(TRAIL_SHADER_SOURCE.to_owned()),
        PostShaderChoice::File(path) => {
            let source = match std::fs::read_to_string(&path) {
                Ok(source) => source,
                // The load already reported a missing path as a warning;
                // repeating it at `ERROR` would contradict the documented
                // severity.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    debug!(?path, "shader.post file is not there; rendering without it");
                    return None;
                }
                Err(err) => {
                    error!(?path, %err, "read shader.post failed; rendering without it");
                    return None;
                }
            };
            match validate_post_shader(&source) {
                Ok(()) => Some(source),
                Err(err) => {
                    error!(?path, %err, "shader.post rejected; rendering without it");
                    None
                }
            }
        }
    }
}

/// Sent right after every attach: the daemon answers `OSC 10/11/12 ; ?`
/// from it, and clears it with the last detach, so a session switch
/// re-reports it to the new session's grid.
pub(crate) fn configure_theme_frame(
    cfg: &RendererConfig,
) -> Result<OutgoingFrame, felis_transport::TransportError> {
    let (fg, bg, cursor) = felis_render_wgpu::palette::configured_theme_report(
        cfg.theme_fg.as_deref(),
        cfg.theme_bg.as_deref(),
        cfg.theme_cursor.as_deref(),
    );
    OutgoingFrame::ordered(&SessionToDaemonMsg::ConfigureTheme { fg, bg, cursor })
}

fn run_client(
    runtime: &tokio::runtime::Runtime,
    startup: ClientStartup,
    mode: AttachMode,
    reconnector: Reconnector,
) -> Result<ExitCode> {
    let mut connection = runtime.block_on(reconnector.dial_launch(LANDING_REMOTE_SPAWN))?;
    let attach = runtime
        .block_on(attach_for_mode(&mut connection, mode))
        .context("attach session on daemon")?;
    // The writer's send gate authorizes every frame against this value; a
    // support report needs it logged when a mixed-vintage pair misbehaves.
    debug!(
        effective_minor = connection.effective_minor,
        "connection negotiated"
    );
    // Demand-driven pacing (docs/explanation/rendering/pipeline.md
    // "Demand-driven emission"): an ssh-carried descriptor declares
    // `false`, since round-trip latency makes per-vsync pulls a poor fit.
    let pull_enabled = reconnector.offer.pull_paced;
    let pump_args = (
        connection.reader,
        connection.writer,
        Arc::new(Mutex::new(connection.driver)),
    );
    // The window keeps no roster; it fetches one per pick.
    drive(
        runtime,
        &attach,
        pump_args,
        reconnector,
        pull_enabled,
        startup,
    )
}

async fn attach_for_mode(
    connection: &mut felis_client_core::CarrierConnection,
    mode: AttachMode,
) -> Result<SessionInfo> {
    match mode {
        AttachMode::Create(args) => Ok(connection.create_with(args).await?),
        // The user named this session; its final screen is a
        // legitimate thing to land on.
        AttachMode::Attach(prefix) => Ok(connection
            .attach_by_prefix(prefix, felis_client_core::AttachIntent::Deliberate)
            .await?),
    }
}

struct ClientStartup {
    warmup: Warmup,
    renderer_cfg: RendererConfig,
    /// Logical pixels; the renderer takes a physical size, and the scale
    /// factor only exists once the window does.
    font_size_logical_px: Option<f32>,
    clipboard_cfg: config::ClipboardConfig,
    window_cfg: config::WindowConfig,
    cursor_cfg: config::CursorConfig,
    shader_cfg: config::ShaderConfig,
    mouse_cfg: config::MouseConfig,
    keymap: Keymap,
    /// Carried into [`App`] so a live reload re-reads the document this
    /// window started from, not whatever platform discovery now finds.
    config_source: ConfigSource,
}

fn drive(
    runtime: &tokio::runtime::Runtime,
    attach: &SessionInfo,
    (reader, writer, driver): (
        FrameReader<CarrierReader>,
        FrameWriter<CarrierWriter>,
        SharedDriver,
    ),
    reconnector: Reconnector,
    pull_enabled: bool,
    startup: ClientStartup,
) -> Result<ExitCode> {
    let ClientStartup {
        mut warmup,
        renderer_cfg,
        font_size_logical_px,
        clipboard_cfg,
        window_cfg,
        cursor_cfg,
        shader_cfg,
        mouse_cfg,
        keymap,
        config_source,
    } = startup;
    info!(
        id = format!("{:#x}", attach.id),
        rows = attach.dims.rows,
        cols = attach.dims.cols,
        "session attached"
    );
    warmup.start_fonts(&renderer_cfg);

    // Region files orphaned by a prior client's crash: this process owns
    // the directory (pid-namespaced), so nothing else clears it.
    felis_client_core::pipe::sweep_temp_dir();

    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .context("build winit event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();

    // Skips silently when the platform has no resolvable config dir.
    if let Some(path) = config_source.path() {
        let watcher_proxy = proxy.clone();
        config_watcher::spawn(runtime.handle(), path, move || {
            watcher_proxy.send_event(AppEvent::ConfigReloaded).is_ok()
        });
    }

    let (input_tx, input_rx) = outgoing_channel();
    let pump = Pumps::spawn(
        reader,
        writer,
        input_rx,
        Arc::clone(&driver),
        proxy.clone(),
        FIRST_CONN_GEN,
        reconnector.clone(),
    );

    // First client→daemon frame: otherwise the daemon answers `OSC 11 ; ?`
    // with xterm white (the "neovim colors look inverted" bug).
    if let Ok(frame) = configure_theme_frame(&renderer_cfg) {
        let _ = input_tx.send(frame);
    }

    let initial_font_size = font_size_logical_px.unwrap_or(DEFAULT_FONT_SIZE_LOGICAL_PX);
    let attached_dims = attach.dims;
    let anchor = RingKey::of(attach);
    let mut app = App {
        config_source,
        runtime: runtime.handle().clone(),
        proxy,
        reconnector,
        surface: None,
        warmup: Some(warmup),
        renderer_cfg,
        outgoing: Some(input_tx),
        pump: Some(pump),
        driver,
        conn_gen: FIRST_CONN_GEN,
        shadow: ShadowScreen::new(attached_dims.rows, attached_dims.cols),
        image_shadow: ImageShadow::new(),
        modifiers: ModifiersState::empty(),
        current_session_id: anchor.id,
        current_sequence: anchor.sequence,
        switch_state: SwitchState::Idle,
        trail: exit_ladder::Trail::default(),
        exit_ladder: None,
        on_transient: false,
        pending_switch: switch_intent::PendingSwitch::default(),
        reconnecting: false,
        terminal_exit: None,
        cursor_cell: None,
        hover_target: None,
        presented_link_target: None,
        cursor_icon: None,
        cursor_px: None,
        held_buttons: HeldButtons::default(),
        clipboard: build_clipboard(&clipboard_cfg),
        clipboard_cfg: clipboard_cfg.clone(),
        drag: DragState::Idle,
        click_streak: ClickStreak::new(),
        configured_font_size_logical_px: font_size_logical_px,
        current_font_size_logical_px: initial_font_size,
        title_prefix: window_cfg.title_prefix.clone(),
        decorations: window_cfg.decorations,
        transparent: window_cfg.clamped_opacity() < 1.0,
        backdrop: window_cfg.backdrop,
        scroll_pixel_accum_y: 0.0,
        last_wheel_at: None,
        wheel_zoom_latched: false,
        scroll_multiplier: mouse_cfg.clamped_scroll_multiplier(),
        // Set from the platform in `resumed` once the window exists.
        scale_factor: 1.0,
        redraw: redraw::RedrawScheduler::new(),
        pull: pull::PullScheduler::new(pull_enabled),
        present_retry: pull::PresentRetry::default(),
        search: SearchUi::default(),
        pending_confirm: None,
        notice: None,
        keymap,
        pipe_state: PipeState::default(),
        pending_reattach: None,
        pending_retarget: None,
        window_focused: true,
        cursor_blink: cursor_blink::BlinkClock::new(
            cursor_cfg.blink,
            cursor_cfg.blink_interval_ms,
            Instant::now(),
        ),
        cursor_cfg,
        cursor_trail: cursor_trail::TrailClock::new(Instant::now()),
        shader_clock: shader_clock::ShaderClock::new(shader_cfg.animate, Instant::now()),
        pointer_px: None,
        last_press_px: None,
        smoke: smoke::Smoke::from_env(),
    };

    // Armed before the loop: the failures that never reach a frame (no
    // adapter, no window, no session) are the ones a gate must catch.
    if let Some(smoke) = app.smoke.as_mut() {
        smoke.arm_watchdog();
    }

    let result = event_loop.run_app(&mut app).context("run event loop");

    // Dropping the pump (in `exiting`) closes the reader/writer channels;
    // the daemon observes EOF and re-pools the session.
    result?;
    let terminal_exit = app.terminal_exit;
    // A clean loop exit is not a smoke pass: a renderer that failed to
    // initialize or a daemon that closed also unwinds without an error.
    if app.smoke.is_some_and(|smoke| !smoke.reached_verdict()) {
        anyhow::bail!("frontend smoke ended without a verified frame");
    }
    Ok(terminal_exit.map_or(ExitCode::SUCCESS, |reason| {
        ExitCode::from(reason.code().get())
    }))
}

/// The event loop's half of the send queue. Pushing never awaits and
/// never blocks the winit loop; the writer task takes what accumulated.
struct OutgoingSender {
    shared: Arc<OutgoingShared>,
}

/// The writer task's half. Held only by that task, so the sender's drop
/// is what tells it the connection is over.
struct OutgoingReceiver {
    shared: Arc<OutgoingShared>,
}

struct OutgoingShared {
    queue: Mutex<OutgoingQueue>,
    /// Woken on every push and once on the sender's drop.
    ready: tokio::sync::Notify,
    closed: AtomicBool,
    /// Latched by the first refused frame, and by a close a switch in
    /// flight absorbed. A refusal loses an ordered message, so the
    /// connection is over from that moment even if the carrier later
    /// drains and the queue stops looking full.
    lost: AtomicBool,
}

fn outgoing_channel() -> (OutgoingSender, OutgoingReceiver) {
    let shared = Arc::new(OutgoingShared {
        queue: Mutex::new(OutgoingQueue::default()),
        ready: tokio::sync::Notify::new(),
        closed: AtomicBool::new(false),
        lost: AtomicBool::new(false),
    });
    (
        OutgoingSender {
            shared: Arc::clone(&shared),
        },
        OutgoingReceiver { shared },
    )
}

impl OutgoingSender {
    /// # Errors
    /// [`OutgoingFull`] when the backlog is past
    /// [`CLIENT_OUTGOING_CAP`](felis_protocol::limits::CLIENT_OUTGOING_CAP),
    /// and for every frame after that one.
    fn send(&self, frame: OutgoingFrame) -> Result<(), OutgoingFull> {
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.shared.lost.load(Ordering::Acquire) {
            return Err(OutgoingFull {
                queued: queue.bytes(),
                incoming: frame.frame().body().len(),
                cap: felis_protocol::limits::CLIENT_OUTGOING_CAP,
            });
        }
        if let Err(full) = queue.push(frame) {
            // Latched under the queue's lock so the answer cannot
            // depend on whether the writer happened to drain in
            // between: the message this refusal dropped is gone, and a
            // connection that lost an ordered message is not one the
            // window may keep using.
            self.shared.lost.store(true, Ordering::Release);
            return Err(full);
        }
        drop(queue);
        self.shared.ready.notify_one();
        Ok(())
    }

    fn is_lost(&self) -> bool {
        self.shared.lost.load(Ordering::Acquire)
    }

    /// The other way this connection ends: a close absorbed by an
    /// in-flight switch. Latching it here is what lets the switch
    /// outcome tell a carrier that is merely idle from one that is gone.
    fn mark_lost(&self) {
        self.shared.lost.store(true, Ordering::Release);
    }
}

impl Drop for OutgoingSender {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.ready.notify_one();
    }
}

impl OutgoingReceiver {
    /// The next queued frame, or `None` once the sender is gone and the
    /// queue has drained (a detach's `Detach` frame still goes out).
    async fn recv(&self) -> Option<OutgoingFrame> {
        loop {
            // Registered before the queue is inspected, so a push that
            // lands between the two is not a lost wakeup.
            let ready = self.shared.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            // Bound to a local so the lock is released before the arm
            // body, which awaits.
            let next = self
                .shared
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop();
            if let Some(frame) = next {
                return Some(frame);
            }
            if self.shared.closed.load(Ordering::Acquire) {
                return None;
            }
            ready.await;
        }
    }
}

/// `resumed` creates both together and `exiting` drops both together:
/// they are never independently `Some`.
struct Surface {
    window: Arc<Window>,
    renderer: Renderer,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent event-loop flags, not a modelable state machine"
)]
struct App {
    /// The document this window was launched against; a live reload
    /// re-reads it rather than re-running platform discovery.
    config_source: ConfigSource,
    /// `Some` only under the smoke environment variable.
    smoke: Option<smoke::Smoke>,
    runtime: tokio::runtime::Handle,
    proxy: EventLoopProxy<AppEvent>,
    /// The carrier this window is on plus the offer it re-declares on
    /// every re-dial.
    reconnector: Reconnector,
    surface: Option<Surface>,
    /// Taken by the first `resumed`.
    warmup: Option<Warmup>,
    renderer_cfg: RendererConfig,
    outgoing: Option<OutgoingSender>,
    /// `None` only briefly between detach and the new pump landing;
    /// `send_input` / `send_control` gate on a missing `outgoing`.
    pump: Option<Pumps>,
    /// Shared with `pump`'s reader task, which validates every inbound
    /// frame against the same table the loop allocates stream ids from.
    driver: SharedDriver,
    /// Bumped by `install_new_session` before the swap and stamped onto
    /// every connection-scoped [`AppEvent`]. `Pumps::abort_reader` alone
    /// cannot carry this: `JoinHandle::abort` takes effect at the task's
    /// next yield, which a reader already returning from EOF has passed.
    conn_gen: u64,
    shadow: ShadowScreen,
    image_shadow: ImageShadow,
    modifiers: ModifiersState,
    /// Updated atomically with the pump swap in `install_new_session`.
    current_session_id: u128,
    /// With [`Self::current_session_id`], this window's [`RingKey`]. A
    /// remembered value rather than a roster lookup, because the session
    /// it names can be reaped while a fetch is in flight (`roster.rs`).
    current_sequence: std::num::NonZeroU64,
    switch_state: SwitchState,
    /// Keeps a burst of chords to a single fetch at a time
    /// (`switch_intent.rs`).
    pending_switch: switch_intent::PendingSwitch,
    /// `None` while the pointer is outside the window or the renderer has
    /// no cell metrics yet.
    cursor_cell: Option<GridPos>,
    /// The re-validated target under the pointer while Ctrl is held,
    /// kept in lockstep with the cursor icon by `update_mouse_cursor_icon`
    /// (`docs/explanation/security-model.md` "OSC 8 hyperlinks and OSC 7
    /// CWD"); drives the Ctrl-hover preview bar.
    hover_target: Option<ActivationTarget>,
    /// The target the last frame that reached the compositor actually
    /// previewed. Activation compares against this rather than against
    /// `hover_target` alone: REQ-910 makes the preview part of the
    /// gesture, and the bottom row changes owner (a search bar closing,
    /// a composition ending) with no event of its own to re-arm on.
    presented_link_target: Option<ActivationTarget>,
    /// The icon last handed to `Window::set_cursor`, so a repeat is not
    /// re-issued; `None` until this window has received one. winit does
    /// not dedupe, and on Wayland each call is a theme lookup plus a
    /// pointer-surface commit.
    cursor_icon: Option<winit::window::CursorIcon>,
    /// 1-based, content-block relative, for the `?1016` SGR-pixel mouse
    /// encoding on button / wheel events, which carry no position.
    cursor_px: Option<(u16, u16)>,
    /// Read on motion to decide `Drag` vs `Motion`, and on release to
    /// route the release the same way as its press.
    held_buttons: HeldButtons,
    /// Rebuilt by `reload_config` when [`Self::clipboard_cfg`] changes.
    clipboard: Arc<dyn Clipboard>,
    /// Held so `reload_config` rebuilds the backend only on a change:
    /// `Arc<dyn Clipboard>` cannot be mutated in place.
    clipboard_cfg: config::ClipboardConfig,
    drag: DragState,
    /// Cycles 1 → 2 → 3 → 1 (a 4th rapid click resets to single, matching
    /// xterm / kitty / alacritty).
    click_streak: ClickStreak,
    /// `None` when the key is unset. The baseline the zoom chords move
    /// away from and Ctrl+Shift+0 returns to.
    configured_font_size_logical_px: Option<f32>,
    /// Logical pixels; the zoom chords move it without touching the
    /// config, and `reload_config` snaps it back.
    current_font_size_logical_px: f32,
    title_prefix: Option<String>,
    /// Applied at window creation via [`window_attributes`] and live via
    /// `Window::set_decorations`, which on macOS can only toggle plain
    /// borderless: a live switch to `false` yields square corners until
    /// the next launch.
    decorations: bool,
    /// `window.opacity < 1.0` at startup. The degree of opacity lives in
    /// the renderer and can change on reload, but crossing the
    /// opaque↔translucent boundary needs a relaunch: winit can't reliably
    /// re-flag an existing surface transparent on every platform.
    transparent: bool,
    /// `blur` is inert unless [`Self::transparent`]; the DWM materials
    /// apply regardless. Every material is a no-op off its own platform.
    backdrop: Backdrop,
    /// Sub-row residue from touchpad `PixelDelta` events; without it every
    /// pixel event collapses to ±1 row, so slow gestures lose sub-row
    /// resolution and fast flicks cap at one row.
    scroll_pixel_accum_y: f64,
    /// Delimits wheel streams via [`WHEEL_STREAM_IDLE`].
    last_wheel_at: Option<Instant>,
    /// Latched at stream start: a Ctrl press that lands during leftover
    /// trackpad inertia must not flip an in-flight scroll into a zoom.
    /// winit 0.30 cannot label inertia events (see
    /// [`wheel_starts_new_stream`]).
    wheel_zoom_latched: bool,
    /// Linux delivers one wheel notch as `LineDelta(0, ±1)` with no OS
    /// scroll acceleration; touchpad pixel-deltas are velocity-scaled
    /// already and are left untouched.
    scroll_multiplier: f64,
    /// Multiplying at every renderer-bound font-size call keeps the
    /// configured logical size stable across display swaps and yields the
    /// `ws_xpixel/ws_ypixel` TIOCGWINSZ values Kitty graphics producers
    /// (yazi) use to pre-scale previews.
    scale_factor: f64,
    /// Handlers flip this instead of calling `window.request_redraw()`;
    /// `about_to_wait` issues one `request_redraw` per iteration.
    redraw: redraw::RedrawScheduler,
    /// Demand-driven pacing (docs/explanation/rendering/pipeline.md
    /// "Demand-driven emission"); inert unless the connection's `Hello`
    /// stated `pull_paced`.
    pull: pull::PullScheduler,
    /// Owned by the window, not the connection: a reconnect rebuilds
    /// `pull` but leaves the surface as unpresentable as it was.
    present_retry: pull::PresentRetry,
    /// REQ-607; the wire surface is shared with the CLI's `sessions
    /// search` (docs/reference/ipc.md "CLI clients").
    search: SearchUi,
    /// docs/explanation/input.md "Confirmation bar": while `Some`, every
    /// keypress routes to the bar.
    pending_confirm: Option<felis_client_core::confirm::PendingConfirm>,
    /// A one-shot refusal to report to the user, painted on the
    /// confirmation bar and cleared by the next keypress. A refusal only
    /// `warn!`ed is invisible: a paste that does nothing is
    /// indistinguishable from an unbound chord or an empty clipboard.
    notice: Option<String>,
    /// Rebuilt on every `reload_config` (docs/reference/config.md).
    keymap: Keymap,
    /// docs/explanation/data-model/scrollback.md "Piping to an external
    /// command": a disconnect closes the window when `Idle` and returns to
    /// the origin when `Active`.
    pipe_state: PipeState,
    /// The daemon counted the push as delivered (the CLI already exited
    /// 0), so dropping it while a switch or pipe handoff is in flight
    /// would lose a switch the user was told succeeded. Last push wins.
    pending_reattach: Option<PendingReattach>,
    /// The retarget analogue of [`Self::pending_reattach`]. At most one
    /// push is parked across both fields: parking either clears the other
    /// (a retarget makes an earlier reattach's session id meaningless,
    /// and a later reattach means the user changed their mind).
    pending_retarget: Option<RetargetTarget>,
    /// docs/explanation/architecture/session-lifecycle.md
    /// "The trail: places this window has been".
    trail: exit_ladder::Trail,
    /// `Some` only between the exit that started an unwind and the
    /// landing that ends it; every landing that installs a session
    /// clears it, which is what restores the ring rung.
    exit_ladder: Option<exit_ladder::ExitLadder>,
    /// Whether the installed session belongs to a pipe/run visit. A
    /// transient is never a place to return to, so a landing that leaves
    /// one records no trail entry.
    on_transient: bool,
    /// Mirrors the renderer's flag so the blink clock can stop (and the
    /// loop park on `ControlFlow::Wait`) while the cursor is hidden.
    window_focused: bool,
    cursor_blink: cursor_blink::BlinkClock,
    /// Retained so a live `reload_config` can reconfigure `cursor_blink`.
    cursor_cfg: config::CursorConfig,
    /// Runs only while a post-process shader is loaded; drops its
    /// `WaitUntil` deadline the moment the trail settles, which keeps an
    /// idle window at zero frames.
    cursor_trail: cursor_trail::TrailClock,
    /// Off unless `shader.animate`; see `shader_clock.rs` for why this is
    /// the one clock nothing arms.
    shader_clock: shader_clock::ShaderClock,
    /// Physical window pixels, `None` outside the window. Distinct from
    /// [`Self::cursor_px`]: the shader contract wants the window, not the
    /// grid.
    pointer_px: Option<PhysicalPosition<f64>>,
    /// Same units; the shader contract reports it so an effect can anchor
    /// to the click.
    last_press_px: Option<PhysicalPosition<f64>>,
    /// Set while the reconnect ladder is re-dialing the carrier and
    /// session the transport dropped; drives the window's disconnected
    /// indicator (docs/explanation/architecture/session-lifecycle.md
    /// "Transport loss").
    reconnecting: bool,
    /// Recorded before `event_loop.exit()` so the process status names
    /// what the log line does; `None` for every ordinary close.
    terminal_exit: Option<ExitReason>,
}

/// Why a window closed with no session to hand back. The window is the
/// wrong surface for the message (felis draws no dialog), so each one
/// is a log line naming the remedy plus a process status
/// (docs/reference/cli.md "Exit codes").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitReason {
    Refused,
    RetriesExhausted,
}

impl ExitReason {
    const fn headline(self) -> &'static str {
        match self {
            Self::Refused => "the daemon refused this window; closing it",
            Self::RetriesExhausted => {
                "no daemon answered within the reconnect budget; closing the window"
            }
        }
    }

    const fn remedy(self) -> &'static str {
        match self {
            Self::Refused | Self::RetriesExhausted => {
                "reattach with `felis attach <id>` once the daemon is back"
            }
        }
    }

    /// A connection this build could not use is a failure to ask (`2`).
    /// A session that ended is not a failure at all: the window takes
    /// the exit ladder and, if that is spent, closes on `0`.
    const fn code(self) -> NonZeroU8 {
        const FAILED_TO_ASK: NonZeroU8 = match NonZeroU8::new(2) {
            Some(code) => code,
            None => NonZeroU8::MIN,
        };
        match self {
            Self::Refused | Self::RetriesExhausted => FAILED_TO_ASK,
        }
    }

    /// `None` for [`felis_client_core::ReconnectError::SessionGone`]:
    /// learning the session ended is the same fact `SessionExited`
    /// carries, so the window unwinds rather than closing on a verdict.
    const fn from_reconnect(err: &felis_client_core::ReconnectError) -> Option<Self> {
        match err {
            felis_client_core::ReconnectError::SessionGone(_) => None,
            felis_client_core::ReconnectError::Refused(_) => Some(Self::Refused),
            felis_client_core::ReconnectError::Exhausted { .. } => Some(Self::RetriesExhausted),
        }
    }
}

/// `InFlight` guards against stacking a second reconnect mid-handshake.
/// `session_gone` rides the variant because it only means anything
/// while a landing is in flight: a dangling sibling flag would mislabel
/// a later, unrelated switch as one with no session to fall back to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum SwitchState {
    #[default]
    Idle,
    InFlight {
        /// The session this window was on has exited or been left for
        /// good, so a failed landing has nothing to stay on and takes
        /// the next rung of the exit ladder.
        session_gone: bool,
        /// B-11's single retry, spent by the refetch it triggers. `None`
        /// for a landing the user named and for one picked from a retry's
        /// roster.
        retry: Option<SwitchDirection>,
    },
}

impl SwitchState {
    const fn in_flight(self) -> bool {
        matches!(self, Self::InFlight { .. })
    }

    /// Whether the landing in flight has no live session to fall back
    /// to, which is also what says the place it is leaving died under it.
    const fn session_gone(self) -> bool {
        matches!(
            self,
            Self::InFlight {
                session_gone: true,
                ..
            }
        )
    }

    /// Reports whether a landing was in flight to observe the exit. The
    /// retry goes with it: a re-pick would fetch its roster on the
    /// connection the exit has already torn down.
    const fn carry_exit(&mut self) -> bool {
        let Self::InFlight {
            session_gone,
            retry,
        } = self
        else {
            return false;
        };
        *session_gone = true;
        *retry = None;
        true
    }
}

/// A `felis sessions switch` push the window could not run when it
/// arrived. The reconnector rides along because session ids are a
/// per-daemon namespace: a push parked across a cross-carrier landing
/// still belongs to the daemon that issued it.
#[derive(Debug, Clone)]
struct PendingReattach {
    reconnector: Reconnector,
    id: u128,
}

impl PendingReattach {
    fn place(&self) -> exit_ladder::Place {
        exit_ladder::Place {
            reconnector: self.reconnector.clone(),
            session_id: self.id,
        }
    }
}

/// docs/explanation/input.md "Which daemon runs the command".
#[derive(Debug, Default)]
enum PipeState {
    #[default]
    Idle,
    /// The reply carries only the bytes, so the sink is held here, which
    /// also keeps a `clipboard` sink on the ungated `write_user` path
    /// (provenance is the request's, not the reply's).
    Awaiting {
        target: felis_client_core::PipeTarget,
        /// Viewport offset the user was at when invoking the pipe.
        viewport: u32,
    },
    Active {
        viewport: u32,
        /// Dropping this unlinks the region file. `None` for `run`.
        region: Option<StagedRegion>,
        /// Captured when the visit starts rather than read off the trail
        /// at return time: a late `SessionExited` prunes the source
        /// entry, and the trail's newest would then be an unrelated
        /// place holding this viewport.
        source: exit_ladder::Place,
    },
    Returning {
        viewport: u32,
        expected: exit_ladder::Place,
    },
}

impl PipeState {
    /// Not `mem::take`: the switch that brings a transient up lands
    /// through the same handler, and taking there would drop the
    /// [`Self::Active`] the chord had just parked, leaking the region
    /// file.
    fn settle_returning(&mut self, landed: &exit_ladder::Place, keep_parked: bool) -> Option<u32> {
        let Self::Returning { viewport, expected } = self else {
            return None;
        };
        let restore = (expected == landed).then_some(*viewport);
        if restore.is_some() || !keep_parked {
            *self = Self::Idle;
        }
        restore
    }

    /// Whether a pipe/run visit still owes a return.
    const fn owes_a_return(&self) -> bool {
        matches!(self, Self::Active { .. } | Self::Returning { .. })
    }

    /// Conditional for the same reason as [`Self::settle_returning`].
    fn take_awaiting(&mut self) -> Option<(felis_client_core::PipeTarget, u32)> {
        let Self::Awaiting { target, viewport } = self else {
            return None;
        };
        let awaited = (target.clone(), *viewport);
        *self = Self::Idle;
        Some(awaited)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClosedOutcome {
    /// Every switch detaches its predecessor, so the daemon closing that
    /// stream is the switch completing, not the daemon dying.
    Ignore,
    /// A switch is mid-handshake on the live connection; exiting here
    /// would race it.
    AwaitSwitch,
    /// A transient pipe/run command exited; unwind to the place it came
    /// from.
    PipeReturn { viewport: u32 },
    /// The transport dropped with nothing in flight: re-dial the same
    /// carrier and session.
    Reconnect,
}

/// The staleness test comes first: a superseded connection's EOF carries
/// no information about the live one. The guards below it stay because
/// a landing can start before the dying session's connection reaches
/// EOF, so that EOF arrives on the still-current generation.
const fn daemon_closed_outcome(
    event_conn_gen: u64,
    live_conn_gen: u64,
    switch_in_flight: bool,
    pipe_state: &PipeState,
) -> ClosedOutcome {
    if event_conn_gen != live_conn_gen {
        return ClosedOutcome::Ignore;
    }
    if switch_in_flight {
        return ClosedOutcome::AwaitSwitch;
    }
    if let PipeState::Active { viewport, .. } = *pipe_state {
        return ClosedOutcome::PipeReturn { viewport };
    }
    ClosedOutcome::Reconnect
}

#[derive(Debug, Default)]
struct SearchUi {
    mode: SearchUiMode,
    query: String,
    /// Daemon emission order is newest-first (`line_index` closer to `-1`
    /// ahead of older scrollback), so `matches[0]` is the youngest hit.
    matches: Vec<SearchHitRecord>,
    /// `None` until the user starts traversing; set to `Some(0)` when the
    /// stream ends clean with matches, so the closest hit auto-jumps.
    current: Option<usize>,
    /// Drives the bar's "searching…" status.
    streaming: bool,
    /// The stream's typed terminal (empty needle, regex compile failure,
    /// size-limit overflow).
    error: Option<String>,
    /// Held so Esc and a re-typed query can `Cancel` it; otherwise the
    /// daemon keeps walking a scrollback nobody is watching.
    stream: Option<StreamId>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum SearchUiMode {
    #[default]
    Off,
    Composing,
    Active,
}

/// `matches` is newest-first, so `Older` advances the index and `Newer`
/// decrements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchDirection {
    /// `n`: move further into scrollback (toward `-∞` line index).
    Older,
    /// `N`: move back toward the live bottom.
    Newer,
}

/// `SearchToClientMsg::Match` minus `byte_spans`: the renderer paints highlight
/// quads off `col_spans`.
#[derive(Debug, Clone)]
struct SearchHitRecord {
    /// REQ-608 row index; negative for scrollback rows.
    line_index: i64,
    /// A match crossing a soft-wrap edge carries one segment per touched
    /// row.
    col_spans: Vec<ColSpan>,
}

/// `Some(bytes)` only on a non-empty `Ime::Commit`; pre-edit text rides
/// the renderer overlay via `preedit_from_ime`. Empty `Commit` strings
/// are dropped: some IMEs send one on cancel (Escape).
fn ime_commit_bytes(ime: &Ime) -> Option<Vec<u8>> {
    match ime {
        Ime::Commit(s) if !s.is_empty() => Some(s.as_bytes().to_vec()),
        Ime::Commit(_) | Ime::Preedit { .. } | Ime::Enabled | Ime::Disabled => None,
    }
}

/// `Some(overlay)` only for a non-empty `Ime::Preedit`, anchored at the
/// shadow screen's cursor cell; everything else clears the overlay.
fn preedit_from_ime(ime: &Ime, cursor_cell: GridPos) -> Option<PreeditOverlay> {
    match ime {
        Ime::Preedit(text, cursor) if !text.is_empty() => Some(PreeditOverlay {
            anchor: (cursor_cell.row, cursor_cell.col),
            text: text.clone(),
            cursor: *cursor,
        }),
        Ime::Preedit(_, _) | Ime::Commit(_) | Ime::Enabled | Ime::Disabled => None,
    }
}

/// One cell at the cursor: the IME panel treats the rect as "the area the
/// user is typing into" and positions its popup adjacent (Wayland
/// `zwp_text_input_v3`, XIM, `AppKit`, Windows TSF all consume it so).
const fn ime_cursor_area_pixels(
    cursor_col: u16,
    cursor_row: u16,
    cell_width: u32,
    cell_height: u32,
) -> (u32, u32, u32, u32) {
    let x = (cursor_col as u32) * cell_width;
    let y = (cursor_row as u32) * cell_height;
    (x, y, cell_width, cell_height)
}

/// Recorded at press time because the routing cannot be re-derived at
/// release: the mouse-mode / Shift predicate ([`should_start_selection`])
/// can flip mid-hold, and either direction breaks the press/release
/// pairing a reporting program relies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressRouting {
    /// Consumed client-side (a selection gesture, Ctrl+Left hyperlink
    /// open, Middle paste); the release must stay silent too.
    Grabbed,
    /// Its release owes the program a matching report.
    Forwarded,
}

/// A `Vec` rather than a map: insertion order is what the motion arm reads
/// to pick the button it reports a drag under.
#[derive(Debug, Default)]
struct HeldButtons(Vec<(MouseButton, PressRouting)>);

impl HeldButtons {
    /// A repeat press with no intervening release replaces the entry:
    /// winit never sends two presses of one button without a release, so
    /// the repeat is proof the release was missed (a `CursorLeft` mid-hold
    /// makes `MouseInput` bail before the release path).
    fn press(&mut self, button: MouseButton, routing: PressRouting) {
        self.0.retain(|(b, _)| *b != button);
        self.0.push((button, routing));
    }

    /// `None` means no press was recorded: the caller drops the release
    /// rather than handing the program a release with no press.
    fn release(&mut self, button: MouseButton) -> Option<PressRouting> {
        let idx = self.0.iter().position(|(b, _)| *b == button)?;
        Some(self.0.remove(idx).1)
    }

    fn is_held(&self, button: MouseButton) -> bool {
        self.0.iter().any(|(b, _)| *b == button)
    }

    const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// What a drag report is attributed to.
    fn last(&self) -> Option<MouseButton> {
        self.0.last().map(|(b, _)| *b)
    }
}

/// "Dragging" vs "settled" is deliberately not a variant split: it is
/// derived from `App::held_buttons`, so a missed release cannot
/// desynchronize two copies of the same fact.
enum DragState {
    Idle,
    /// A single click landed here; the first drag motion into a new cell
    /// promotes it, a plain release discards it (xterm / alacritty
    /// convention). The mode is fixed at press time: Alt+press anchors a
    /// Rectangle.
    Pending(GridPos, SelectionMode),
    /// Extending while a starting button is held, persisting after
    /// release for the copy path.
    Selected(Selection),
}

/// 500 ms matches xterm's default `multiClickTime` and kitty / alacritty.
const CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// Cycles 1 → 2 → 3 → 1: single-click drag anchor, word
/// (`Selection::word_at`), row (`Selection::line_at`).
#[derive(Debug)]
struct ClickStreak {
    last_at: Option<Instant>,
    last_cell: Option<GridPos>,
    count: u8,
}

impl ClickStreak {
    const fn new() -> Self {
        Self {
            last_at: None,
            last_cell: None,
            count: 0,
        }
    }

    fn record(&mut self, now: Instant, cell: GridPos) -> u8 {
        let same_cell = self.last_cell == Some(cell);
        let in_window = self
            .last_at
            .is_some_and(|t| now.duration_since(t) <= CLICK_INTERVAL);
        let next = if same_cell && in_window {
            (self.count % 3) + 1
        } else {
            1
        };
        self.last_at = Some(now);
        self.last_cell = Some(cell);
        self.count = next;
        next
    }

    /// Called when the program grabs the press.
    const fn reset(&mut self) {
        self.last_at = None;
        self.last_cell = None;
        self.count = 0;
    }
}

/// Mouse mode on but Shift held is the xterm convention for "override the
/// program's mouse capture, this drag is mine".
const fn should_start_selection(mouse_mode_active: bool, shift_held: bool) -> bool {
    !mouse_mode_active || shift_held
}

/// A press is the "click elsewhere" gesture whether or not the program
/// consumes it. Middle is the exception: its no-mouse-mode meaning is
/// "paste PRIMARY", which xterm defines as independent of selection
/// state.
const fn forwarded_press_dismisses_selection(button: MouseButton) -> bool {
    matches!(button, MouseButton::Left | MouseButton::Right)
}

/// True if scrollback entered under the highlight or the `?1049` alt-screen
/// swapped. `alt_screen_before`/`alt_screen_now` bracket `Shadow::apply`
/// because `ModeFlags` resends unchanged alt-screen bits on unrelated mode
/// updates; only an actual transition indicates a screen swap.
const fn grid_change_invalidates_selection(
    msg: &GridMsg,
    alt_screen_before: bool,
    alt_screen_now: bool,
) -> bool {
    alt_screen_before != alt_screen_now || viewport_message_enters_scrollback(msg)
}

/// The selection model stores visible-row coordinates
/// (`felis-client-core::selection::Selection`), so scrolling into
/// history drops the stale highlight, matching xterm. Snap-back
/// (`lines_from_bottom == 0`) does not invalidate.
const fn viewport_message_enters_scrollback(msg: &GridMsg) -> bool {
    match msg {
        GridMsg::ViewportState {
            lines_from_bottom, ..
        } => *lines_from_bottom != 0,
        _ => false,
    }
}

const fn map_button(button: WinitMouseButton) -> Option<MouseButton> {
    match button {
        WinitMouseButton::Left => Some(MouseButton::Left),
        WinitMouseButton::Right => Some(MouseButton::Right),
        WinitMouseButton::Middle => Some(MouseButton::Middle),
        // xterm only encodes buttons 8/9/10/11; a code for anything else
        // would be mis-handled by the running program.
        WinitMouseButton::Back | WinitMouseButton::Other(8) => Some(MouseButton::Button8),
        WinitMouseButton::Forward | WinitMouseButton::Other(9) => Some(MouseButton::Button9),
        WinitMouseButton::Other(10) => Some(MouseButton::Button10),
        WinitMouseButton::Other(11) => Some(MouseButton::Button11),
        WinitMouseButton::Other(_) => None,
    }
}

/// Matches xterm's `alternateScroll` (default-on since patch #221),
/// alacritty's `scrolling.alternate_scroll`, and kitty: the wheel must do
/// something in `less` / `man` / `vim`, which sit on the alt screen
/// without mouse mode. Primary-screen wheel scrolls scrollback, as every
/// modern terminal does; xterm's "drop" is the outlier.
#[derive(Debug, PartialEq, Eq)]
enum WheelEncoding {
    /// `?1000` / `?1002` / `?1003`: raw buttons via [`wheel_buttons`].
    MouseButtons,
    /// Alt-screen pager: `ESC [ A` / `ESC [ B` via [`wheel_arrow_bytes`].
    ArrowKeys,
    /// Primary screen, no mouse mode: `InputMsg::Viewport`.
    Scroll,
}

/// The `Scroll` arm needs no viewport state: the daemon clamps an
/// over-deep request.
const fn wheel_encoding_for(mouse_mode_active: bool, on_alternate_screen: bool) -> WheelEncoding {
    if mouse_mode_active {
        WheelEncoding::MouseButtons
    } else if on_alternate_screen {
        WheelEncoding::ArrowKeys
    } else {
        WheelEncoding::Scroll
    }
}

/// `f64::signum` returns `1.0` for positive zero (IEEE 754), which would
/// synthesize a spurious tick on an axis with no motion.
fn signum_or_zero(v: f64) -> f64 {
    if v == 0.0 {
        0.0
    } else if v > 0.0 {
        1.0
    } else {
        -1.0
    }
}

/// Positive = up. Line-deltas round to nearest (≥ 1 if non-zero);
/// pixel-deltas collapse to ±1 so touchpad smooth-scroll doesn't
/// synthesize a swarm of events per swipe. Shared by
/// [`wheel_arrow_bytes`] and the Ctrl+wheel zoom so both count a notch
/// the same way.
fn wheel_y_ticks(delta: MouseScrollDelta) -> i32 {
    let dy = match delta {
        MouseScrollDelta::LineDelta(_, y) => f64::from(y),
        MouseScrollDelta::PixelDelta(p) => signum_or_zero(p.y),
    };
    let mag = dy.abs();
    if mag == 0.0 {
        return 0;
    }
    let count = (mag.round() as i32).max(1);
    if dy > 0.0 { count } else { -count }
}

/// Positive scrolls into history. `LineDelta` scales by `line_multiplier`
/// and clears `pixel_accum_y` so device switches discard stale residue.
/// `PixelDelta` ignores the multiplier (touchpad deltas are already
/// velocity-scaled) and accumulates sub-cell pixels across events.
fn wheel_pixels_to_rows(
    delta: MouseScrollDelta,
    cell_height_px: f64,
    line_multiplier: f64,
    pixel_accum_y: &mut f64,
) -> i32 {
    let cell_height_px = cell_height_px.max(1.0);
    match delta {
        MouseScrollDelta::LineDelta(_, y) => {
            *pixel_accum_y = 0.0;
            (f64::from(y) * line_multiplier).round() as i32
        }
        MouseScrollDelta::PixelDelta(p) => {
            *pixel_accum_y += p.y;
            let rows = (*pixel_accum_y / cell_height_px).trunc() as i32;
            *pixel_accum_y = f64::mul_add(f64::from(rows), -cell_height_px, *pixel_accum_y);
            rows
        }
    }
}

/// Shift+wheel sends `PageUp` / `PageDown`, as xterm `alternateScroll`,
/// alacritty, and kitty do. Always CSI form regardless of DECCKM,
/// matching `encode_named`, so wheel- and key-driven arrows produce
/// identical bytes.
fn wheel_arrow_bytes(delta: MouseScrollDelta, shift_held: bool) -> Vec<u8> {
    let ticks = wheel_y_ticks(delta);
    if ticks == 0 {
        return Vec::new();
    }
    let seq: &[u8] = match (ticks > 0, shift_held) {
        (true, false) => b"\x1b[A",
        (false, false) => b"\x1b[B",
        (true, true) => b"\x1b[5~",
        (false, true) => b"\x1b[6~",
    };
    seq.repeat(ticks.unsigned_abs() as usize)
}

/// Browser / kitty / alacritty convention: one notch = ±1 px. `0.0` when
/// there is no vertical motion, so the modifier alone never zooms.
fn wheel_zoom_delta_px(delta: MouseScrollDelta) -> f32 {
    wheel_y_ticks(delta) as f32
}

/// Stream events arrive within frames; new gestures follow a human-scale
/// pause. On macOS, winit collapses trackpad inertia onto ordinary
/// `MouseWheel` events without distinct phase data, leaving the inter-event
/// gap as the only signal.
const WHEEL_STREAM_IDLE: Duration = Duration::from_millis(100);

/// Whether the zoom-vs-scroll decision must be taken afresh from the live
/// modifiers (`None` = first event ever).
fn wheel_starts_new_stream(since_prev: Option<Duration>) -> bool {
    since_prev.is_none_or(|gap| gap >= WHEEL_STREAM_IDLE)
}

fn wheel_buttons(delta: MouseScrollDelta) -> Vec<MouseButton> {
    // xterm's encoding is one button per tick: N events for line-deltas
    // ≥ 1.0, a single event for pixel-deltas.
    let (x, y) = match delta {
        MouseScrollDelta::LineDelta(x, y) => (f64::from(x), f64::from(y)),
        MouseScrollDelta::PixelDelta(p) => (signum_or_zero(p.x), signum_or_zero(p.y)),
    };
    let mut out = Vec::new();
    let yticks = y.abs().round() as u32;
    let yticks = yticks.max(u32::from(y.abs() > 0.0));
    for _ in 0..yticks {
        out.push(if y > 0.0 {
            MouseButton::WheelUp
        } else {
            MouseButton::WheelDown
        });
    }
    let xticks = x.abs().round() as u32;
    let xticks = xticks.max(u32::from(x.abs() > 0.0));
    for _ in 0..xticks {
        out.push(if x > 0.0 {
            MouseButton::WheelRight
        } else {
            MouseButton::WheelLeft
        });
    }
    out
}

/// Every later connection is `install_new_session`'s increment of it.
const FIRST_CONN_GEN: u64 = 0;

struct Pumps {
    reader_task: tokio::task::JoinHandle<()>,
    /// Held so the writer task isn't dropped while the pump is live; it
    /// exits when its input channel closes.
    writer_task: tokio::task::JoinHandle<()>,
}

/// How long a retired connection's writer may take to push the queued
/// `Detach` before [`Pumps::retire`] aborts it.
const RETIRED_WRITER_GRACE: Duration = Duration::from_secs(5);

/// A mutex and not a channel because both sides need the same table: an
/// id the loop allocates must already be live in the table the reader
/// validates against before that stream's first item can arrive. The lock
/// is held only across `classify` / `decode`, never across an await.
pub(crate) type SharedDriver = Arc<Mutex<ClientDriver>>;

impl Pumps {
    fn spawn(
        mut reader: FrameReader<CarrierReader>,
        mut writer: FrameWriter<CarrierWriter>,
        input_rx: OutgoingReceiver,
        driver: SharedDriver,
        proxy: EventLoopProxy<AppEvent>,
        conn_gen: u64,
        reconnector: Reconnector,
    ) -> Self {
        let reader_proxy = proxy;
        let reader_task = tokio::spawn(async move {
            loop {
                match reader.next_frame().await {
                    Ok(Some(frame)) => {
                        if !forward_frame(&driver, &frame, &reader_proxy, conn_gen, &reconnector) {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        warn!(?err, "daemon read");
                        break;
                    }
                }
            }
            drop(reader_proxy.send_event(AppEvent::DaemonClosed { conn_gen }));
        });

        let writer_task = tokio::spawn(async move {
            while let Some(out) = input_rx.recv().await {
                if let Err(err) = writer.send_checked_unflushed(out.frame()).await {
                    warn!(?err, "daemon write");
                    break;
                }
                if let Err(err) = writer.flush().await {
                    warn!(?err, "daemon flush");
                    break;
                }
            }
        });

        Self {
            reader_task,
            writer_task,
        }
    }

    /// An optimization, not the correctness barrier: `abort` only takes
    /// effect at the task's next yield, so [`App::conn_gen`] is what keeps
    /// in-hand frames out of the new shadow. The writer task drains
    /// naturally so the queued `Detach` still goes out.
    fn abort_reader(&self) {
        self.reader_task.abort();
    }

    /// Retire a replaced connection: abort reader immediately and give the
    /// writer [`RETIRED_WRITER_GRACE`] to flush `Detach` before aborting.
    /// Dropping handles only detaches tasks, leaking parked writers and
    /// queues indefinitely across dead links.
    fn retire(self) {
        self.reader_task.abort();
        let mut writer_task = self.writer_task;
        tokio::spawn(async move {
            if tokio::time::timeout(RETIRED_WRITER_GRACE, &mut writer_task)
                .await
                .is_err()
            {
                warn!("retired connection's writer did not drain; aborting it");
                writer_task.abort();
            }
        });
    }
}

/// Returns `false` to stop the reader loop, which then posts
/// [`AppEvent::DaemonClosed`], so a refused frame ends the connection
/// exactly as a hangup does (A-7): tolerating it would leave the window
/// painting a grid the daemon and the client had stopped agreeing about.
fn forward_frame(
    driver: &SharedDriver,
    frame: &OwnedFrame,
    proxy: &EventLoopProxy<AppEvent>,
    conn_gen: u64,
    reconnector: &Reconnector,
) -> bool {
    let mut guard = driver.lock().unwrap_or_else(PoisonError::into_inner);
    let incoming = match guard.classify(frame) {
        Ok(incoming) => incoming,
        Err(err) => {
            warn!(?err, "daemon frame refused; closing the connection");
            return false;
        }
    };
    let payload = match incoming {
        // The search bar is the only stream this window opens.
        Incoming::Control(ConnToClientMsg::End { stream_id, count }) => {
            drop(guard);
            return proxy
                .send_event(AppEvent::SearchEnded {
                    stream_id,
                    outcome: Ok(count),
                    conn_gen,
                })
                .is_ok();
        }
        Incoming::Control(ConnToClientMsg::Error {
            subject: Subject::Stream(stream_id),
            reason,
            detail,
        }) => {
            drop(guard);
            return proxy
                .send_event(AppEvent::SearchEnded {
                    stream_id,
                    outcome: Err(format!("{}: {detail}", reason.as_str())),
                    conn_gen,
                })
                .is_ok();
        }
        // A-5 makes a typed refusal the request's answer, not the
        // connection's end; the id tells the pipe region request from the
        // roster fetch.
        Incoming::Control(ConnToClientMsg::Error {
            subject: Subject::Request(request),
            reason,
            detail,
        }) => {
            drop(guard);
            return proxy
                .send_event(AppEvent::RequestRefused {
                    request,
                    detail: format!("{}: {detail}", reason.as_str()),
                    conn_gen,
                })
                .is_ok();
        }
        // The handshake arms are refused by phase, and `Conn::Refused` is
        // written only before an attach; A-7 leaves no skip arm.
        Incoming::Control(msg) => {
            warn!(
                ?msg,
                "unexpected Conn frame from daemon; closing the connection"
            );
            return false;
        }
        // The cancel raced the terminal that already closed the stream.
        Incoming::CancelIgnored { .. } => return true,
        Incoming::Payload(payload) => payload,
    };
    macro_rules! decoded {
        ($ty:ty) => {
            match guard.decode::<$ty>(&payload) {
                Ok(Delivery::Deliver(delivered)) => delivered,
                // The cancel and the item crossed on the wire.
                Ok(Delivery::DroppedAfterCancel | Delivery::RefuseStream { .. }) => return true,
                Err(err) => {
                    warn!(?err, "daemon frame refused; closing the connection");
                    return false;
                }
            }
        };
    }
    match payload.kind {
        MessageKind::Grid => {
            let msg = decoded!(GridMsg).msg;
            drop(guard);
            proxy
                .send_event(AppEvent::GridFrame { msg, conn_gen })
                .is_ok()
        }
        MessageKind::Search => {
            let msg = decoded!(SearchToClientMsg).msg;
            drop(guard);
            proxy
                .send_event(AppEvent::SearchFrame { msg, conn_gen })
                .is_ok()
        }
        MessageKind::Image => {
            let msg = decoded!(ImageMsg).msg;
            drop(guard);
            proxy
                .send_event(AppEvent::ImageFrame { msg, conn_gen })
                .is_ok()
        }
        MessageKind::Region => {
            // The decode retires the id a `Reply` names: left
            // outstanding, a later frame echoing it would pass as this
            // reply.
            let msg = decoded!(RegionToClientMsg).msg;
            drop(guard);
            match msg {
                RegionToClientMsg::Reply { data, position, .. } => proxy
                    .send_event(AppEvent::PipeRegionReady { data, position })
                    .is_ok(),
                // A row belongs to a `Rows` stream this window never
                // opened; A-7 leaves no skip arm.
                other => {
                    warn!(?other, "off-place Region frame; closing the connection");
                    false
                }
            }
        }
        MessageKind::Push => {
            let msg = decoded!(PushMsg).msg;
            drop(guard);
            match msg {
                PushMsg::Evicted { reason } => {
                    tracing::info!(%reason, "evicted by `felis sessions evict`; window closing");
                    proxy.send_event(AppEvent::Detached).is_ok()
                }
                PushMsg::Reattach { id } => {
                    tracing::info!(id = %SessionHex(id), "reattach push from daemon");
                    proxy
                        .send_event(AppEvent::ReattachRequested {
                            reconnector: reconnector.clone(),
                            id,
                        })
                        .is_ok()
                }
                PushMsg::SessionExited { id } => {
                    tracing::info!(id = %SessionHex(id), "shell exited; session ended");
                    proxy
                        .send_event(AppEvent::SessionExited {
                            conn_gen,
                            place: exit_ladder::Place {
                                reconnector: reconnector.clone(),
                                session_id: id,
                            },
                        })
                        .is_ok()
                }
                PushMsg::RetargetHost { target, .. } => {
                    tracing::info!(
                        carrier = target.carrier.label(),
                        "retarget push from daemon"
                    );
                    proxy
                        .send_event(AppEvent::RetargetRequested { target })
                        .is_ok()
                }
            }
        }
        // The daemon admits `Ops` post-attach so a window can re-list
        // without dialing a second connection (B-11).
        MessageKind::Ops => {
            // An uncorrelated listing is not a reply: taken as one it would
            // hand the daemon the window's pick, so A-7 closes the
            // connection rather than skipping it.
            let delivered = decoded!(OpsToClientMsg);
            let request = delivered.request();
            drop(guard);
            match delivered.msg {
                OpsToClientMsg::Listed { sessions } => proxy
                    .send_event(AppEvent::RosterListed {
                        request,
                        sessions,
                        conn_gen,
                    })
                    .is_ok(),
                // `List` is the only Ops verb a window issues; A-7 leaves
                // no skip arm.
                other => {
                    warn!(?other, "off-place Ops frame; closing the connection");
                    false
                }
            }
        }
        // `Session` is read by the dial path's own round trips; `Notify`
        // and `Input` a `Window` connection is not admitted at all.
        MessageKind::Session | MessageKind::Notify | MessageKind::Input => {
            warn!(
                kind = payload.kind.as_str(),
                "off-place frame family; closing the connection"
            );
            false
        }
        MessageKind::Conn => unreachable!("the control family was handled above"),
    }
}

#[cfg(test)]
mod tests;

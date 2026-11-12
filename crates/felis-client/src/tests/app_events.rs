//! `App` driven through [`App::on_app_event`] with no event loop, no
//! GPU and no daemon: the daemon's side is played by hand, every event
//! the window posts to itself lands in [`AppHarness::posted`], and every
//! window effect in [`AppHarness::window`].

use std::sync::mpsc;

use felis_client_core::{PipeRegionSource, PipeTarget};

use super::*;
use crate::window_input::{KeyPress, PressedKey, WindowInput};

#[derive(Default)]
struct RecordedWindow {
    titles: Mutex<Vec<String>>,
    attention_requests: Mutex<u32>,
}

impl RecordedWindow {
    fn last_title(&self) -> Option<String> {
        self.titles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last()
            .cloned()
    }

    fn attention_requests(&self) -> u32 {
        *self
            .attention_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl WindowHost for RecordedWindow {
    fn set_title(&self, title: &str) {
        self.titles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(title.to_owned());
    }
    fn set_cursor(&self, _: winit::window::CursorIcon) {}
    fn request_attention(&self) {
        *self
            .attention_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner) += 1;
    }
    fn request_redraw(&self) {}
    fn set_ime_cursor_area(&self, _: PhysicalPosition<f64>, _: PhysicalSize<u32>) {}
    fn inner_size(&self) -> PhysicalSize<u32> {
        PhysicalSize::new(960, 600)
    }
    fn prefers_dark(&self) -> Option<bool> {
        None
    }
    fn toggle_fullscreen(&self) {}
    fn set_decorations(&self, _: bool) {}
    fn set_option_as_alt(&self, _: bool) {}
    fn apply_backdrop(&self, _: Backdrop, _: bool) {}
    fn repair_translucent_chrome(&self) {}
    fn refresh_interval(&self) -> Duration {
        Duration::from_millis(16)
    }
}

struct AppHarness {
    app: App,
    posted: mpsc::Receiver<AppEvent>,
    window: Arc<RecordedWindow>,
    outgoing: OutgoingReceiver,
}

impl AppHarness {
    /// Drains what the window queued for the daemon since the last
    /// call, as the writer task would.
    fn sent(&self) -> Vec<felis_transport::CheckedFrame> {
        let mut queue = self
            .outgoing
            .shared
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        std::iter::from_fn(|| queue.pop())
            .map(|frame| frame.frame().clone())
            .collect()
    }

    /// Blocks for a background dial's outcome, which is what the
    /// winit loop would deliver next.
    fn next_posted(&self) -> AppEvent {
        self.posted
            .recv_timeout(Duration::from_secs(10))
            .expect("the window posted an event")
    }
}

/// A socket no daemon listens on, so a dial to it fails at connect
/// without leaving the machine.
fn absent_daemon(tag: &str) -> Reconnector {
    let pid = std::process::id();
    #[cfg(unix)]
    let path = std::env::temp_dir().join(format!("felis-test-absent-{pid}-{tag}.sock"));
    #[cfg(windows)]
    let path = PathBuf::from(format!(r"\\.\pipe\felis-test-absent-{pid}-{tag}"));
    Reconnector {
        carrier: Carrier::Local(path.into()),
        offer: Offer::window(true),
    }
}

fn session(id: u128) -> SessionInfo {
    SessionInfo {
        id,
        dims: felis_protocol::messages::GridDims {
            rows: 24,
            cols: 80,
            pixel_w: 0,
            pixel_h: 0,
        },
        title: None,
        cwd: None,
        spawn_cwd: None,
        terminal_progress: None,
        idle_seconds: None,
        tags: Vec::new(),
        last_notification: None,
        foreground: None,
        exited: false,
        last_exit_code: None,
        attachments: Vec::new(),
        sequence: std::num::NonZeroU64::MIN,
        program_status: Vec::new(),
    }
}

fn app_on(runtime: &tokio::runtime::Runtime, reconnector: Reconnector, id: u128) -> AppHarness {
    let (tx, posted) = mpsc::channel();
    let tx = Mutex::new(tx);
    let events = EventSink::new(move |event| {
        tx.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(event)
            .is_ok()
    });
    let cfg = config::EffectiveConfig::default();
    let startup = ClientStartup {
        warmup: None,
        startup_token: startup_token::StartupToken::default(),
        renderer_cfg: renderer_config_from(&cfg),
        post_animations: Vec::new(),
        font_size_logical_px: configured_font_size_logical_px(&cfg),
        clipboard_cfg: config::ClipboardConfig {
            use_os_clipboard: false,
            ..cfg.clipboard
        },
        window_cfg: cfg.window,
        cursor_cfg: cfg.cursor,
        shader_cfg: cfg.shader.clone(),
        mouse_cfg: cfg.mouse,
        shell_cfg: cfg.shell.clone(),
        keymap: Keymap::default_for_platform(),
        config_source: ConfigSource::Default,
        window_class: None,
    };
    let (outgoing, outgoing_rx) = outgoing_channel();
    let link = AppLink {
        runtime: runtime.handle().clone(),
        events,
        reconnector,
        outgoing,
        pump: None,
        driver: Arc::new(Mutex::new(ClientDriver::client(Offer::window(true).mode))),
        effective_minor: 0,
        pull_enabled: true,
    };
    let window = Arc::new(RecordedWindow::default());
    let mut app = App::new(startup, &session(id), link);
    let host: Arc<dyn WindowHost> = window.clone();
    app.window = Some(host);
    AppHarness {
        app,
        posted,
        window,
        outgoing: outgoing_rx,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("test runtime")
}

/// Lands a switch on a fresh in-process carrier, holding the far end
/// so the new connection does not see EOF.
#[cfg(unix)]
fn land_switch(
    runtime: &tokio::runtime::Runtime,
    harness: &mut AppHarness,
    id: u128,
) -> tokio::net::UnixStream {
    land_switch_at(runtime, harness, id, 0)
}

#[cfg(unix)]
fn land_switch_at(
    runtime: &tokio::runtime::Runtime,
    harness: &mut AppHarness,
    id: u128,
    effective_minor: u16,
) -> tokio::net::UnixStream {
    let _entered = runtime.enter();
    let (near, far) = tokio::net::UnixStream::pair().expect("socket pair");
    let (read, write) = near.into_split();
    let driver = Arc::clone(&harness.app.driver);
    harness.app.install_new_session(
        FrameReader::new(CarrierReader::Local(read)),
        FrameWriter::new(CarrierWriter::Local(write), effective_minor),
        driver,
        &session(id),
        true,
    );
    far
}

fn input_frame(msg: &InputMsg) -> felis_transport::CheckedFrame {
    OutgoingFrame::input(msg).expect("encodes").frame().clone()
}

mod announce;
mod paste_pipe;
mod switch;

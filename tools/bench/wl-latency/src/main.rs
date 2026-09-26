//! Calibration harness for a Wayland keystroke-to-pixel latency instrument:
//! it injects into its own surface and times its own answer, so the numbers
//! bound what such an instrument could resolve before any terminal is measured.
//! `docs/explanation/benchmarks.md`, "Input latency is borrowed, not invented".

use std::io::Write as _;
use std::os::fd::AsFd;
use std::process::Command;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, poll};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{
    Connection, Dispatch, DispatchError, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

const APP_ID: &str = "felis-wl-latency";
const KEY_A: u32 = 30;
/// Typometer types `.` and nothing else, so the Wayland field types it too.
const KEY_PERIOD: u32 = 52;
const KEY_BACKSPACE: u32 = 14;

/// Self-contained so the compositor never has to resolve an xkb include path.
const KEYMAP: &str = r#"xkb_keymap {
xkb_keycodes "wl-latency" {
    minimum = 8;
    maximum = 255;
    <A> = 38;
    <PER> = 60;
    <BKS> = 22;
};
xkb_types "wl-latency" {
    type "ONE_LEVEL" {
        modifiers = none;
        map[none] = Level1;
        level_name[Level1] = "Any";
    };
};
xkb_compatibility "wl-latency" { };
xkb_symbols "wl-latency" {
    key <A> { type = "ONE_LEVEL", [ a ] };
    key <PER> { type = "ONE_LEVEL", [ period ] };
    key <BKS> { type = "ONE_LEVEL", [ BackSpace ] };
};
};
"#;

#[derive(PartialEq)]
enum FrameState {
    Waiting,
    Ready,
    Failed,
}

struct Capture {
    buffer: wl_buffer::WlBuffer,
    map: memmap2::MmapMut,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
}

struct State {
    globals: Vec<(String, u32)>,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    vk_manager: Option<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>,
    screencopy: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    output: Option<wl_output::WlOutput>,
    surface: Option<wl_surface::WlSurface>,
    black: Option<wl_buffer::WlBuffer>,
    white: Option<wl_buffer::WlBuffer>,
    configured: bool,
    size: (i32, i32),
    focused: bool,
    /// Set by the surface's own key handler; the flip is the reply to a key.
    flips: u32,
    flip_at: Option<Instant>,
    frame: FrameState,
    capture: Option<Capture>,
}

impl State {
    fn new() -> Self {
        Self {
            globals: Vec::new(),
            compositor: None,
            shm: None,
            wm_base: None,
            seat: None,
            vk_manager: None,
            screencopy: None,
            output: None,
            surface: None,
            black: None,
            white: None,
            configured: false,
            size: (0, 0),
            focused: false,
            flips: 0,
            flip_at: None,
            frame: FrameState::Waiting,
            capture: None,
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        state.globals.push((interface.clone(), version));
        let take = |want: u32| version.min(want);
        match interface.as_str() {
            "wl_compositor" => {
                state.compositor = Some(registry.bind(name, take(4), qh, ()));
            }
            "wl_shm" => state.shm = Some(registry.bind(name, take(1), qh, ())),
            "xdg_wm_base" => state.wm_base = Some(registry.bind(name, take(3), qh, ())),
            "wl_seat" => state.seat = Some(registry.bind(name, take(7), qh, ())),
            "wl_output" if state.output.is_none() => {
                state.output = Some(registry.bind(name, take(3), qh, ()));
            }
            "zwp_virtual_keyboard_manager_v1" => {
                state.vk_manager = Some(registry.bind(name, 1, qh, ()));
            }
            "zwlr_screencopy_manager_v1" => {
                state.screencopy = Some(registry.bind(name, take(3), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Configure { width, height, .. } = event
            && width > 0
            && height > 0
        {
            state.size = (width, height);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
            && caps.contains(wl_seat::Capability::Keyboard)
        {
            seat.get_keyboard(qh, ());
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { .. } => state.focused = true,
            wl_keyboard::Event::Leave { .. } => state.focused = false,
            wl_keyboard::Event::Key {
                state: WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => {
                let (Some(surface), Some(white)) = (&state.surface, &state.white) else {
                    return;
                };
                surface.attach(Some(white), 0, 0);
                surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
                surface.commit();
                state.flips += 1;
                state.flip_at = Some(Instant::now());
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                let fits = state.capture.as_ref().is_some_and(|c| {
                    c.width == width
                        && c.height == height
                        && c.stride == stride
                        && c.format == format as u32
                });
                if !fits {
                    state.capture =
                        Some(make_capture(state, format, width, height, stride, qh).unwrap());
                }
                if frame.version() < 3 {
                    frame.copy(&state.capture.as_ref().unwrap().buffer);
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                frame.copy(&state.capture.as_ref().unwrap().buffer);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.frame = FrameState::Ready,
            zwlr_screencopy_frame_v1::Event::Failed => state.frame = FrameState::Failed,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_output::WlOutput);
delegate_noop!(State: ignore zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);
delegate_noop!(State: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

fn shm_file(len: usize) -> std::io::Result<std::fs::File> {
    let file = tempfile::tempfile()?;
    file.set_len(len as u64)?;
    Ok(file)
}

fn make_capture(
    state: &State,
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
    qh: &QueueHandle<State>,
) -> std::io::Result<Capture> {
    let len = (stride * height) as usize;
    let file = shm_file(len)?;
    let pool = state
        .shm
        .as_ref()
        .expect("shm")
        .create_pool(file.as_fd(), len as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        qh,
        (),
    );
    pool.destroy();
    // SAFETY: the mapping is read only between a frame's `ready` and the next
    // `copy` into it, and its contents are copied out before the next capture
    // is requested, so no read races the compositor's write.
    let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
    Ok(Capture {
        buffer,
        map,
        width,
        height,
        stride,
        format: format as u32,
    })
}

/// A pair of never-mutated buffers: the compositor may still hold the one it
/// showed last, and alternating between two immutable buffers is the cheapest
/// way to never write a buffer that is still on screen.
fn make_surface_buffers(
    state: &mut State,
    qh: &QueueHandle<State>,
    width: i32,
    height: i32,
    marker: (i32, i32, i32),
) -> std::io::Result<()> {
    let stride = width * 4;
    let len = (stride * height) as usize;
    for white in [false, true] {
        let file = shm_file(len)?;
        // SAFETY: the mapping is filled and flushed before the buffer is handed
        // to the compositor and is then leaked without ever being read or
        // written again, so no reference aliases a buffer the compositor holds.
        let mut map = unsafe { memmap2::MmapMut::map_mut(&file)? };
        map.fill(0);
        for px in map.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[0, 0, 0, 255]);
        }
        if white {
            let (mx, my, size) = marker;
            for y in my..(my + size).min(height) {
                let row = (y * stride) as usize;
                for x in mx..(mx + size).min(width) {
                    let at = row + (x * 4) as usize;
                    map[at..at + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
        }
        map.flush()?;
        let pool = state
            .shm
            .as_ref()
            .expect("shm")
            .create_pool(file.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(0, width, height, stride, wl_shm::Format::Xrgb8888, qh, ());
        pool.destroy();
        if white {
            state.white = Some(buffer);
        } else {
            state.black = Some(buffer);
        }
        std::mem::forget(map);
        std::mem::forget(file);
    }
    Ok(())
}

struct Instrument {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
}

impl Instrument {
    fn roundtrip(&mut self) {
        while let Err(err) = self.queue.roundtrip(&mut self.state) {
            self.wait_writable(err, "roundtrip");
        }
    }

    fn dispatch(&mut self) {
        while let Err(err) = self.queue.blocking_dispatch(&mut self.state) {
            self.wait_writable(err, "dispatch");
        }
    }

    /// Both calls flush before they read, and a compositor busy enough to
    /// leave the socket's send buffer full makes that flush fail with
    /// `WouldBlock` rather than wait, so the pending requests are retried
    /// once the socket drains; any other error is fatal.
    fn wait_writable(&self, err: DispatchError, what: &str) {
        let DispatchError::Backend(WaylandError::Io(io)) = &err else {
            panic!("{what}: {err:?}");
        };
        if io.kind() != std::io::ErrorKind::WouldBlock {
            panic!("{what}: {err:?}");
        }
        let fd = self.conn.as_fd();
        let mut fds = [PollFd::new(&fd, PollFlags::OUT)];
        match poll(&mut fds, None) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => panic!("{what}: poll: {e}"),
        }
    }

    /// One screencopy frame of `region`, as raw pixels.
    fn capture(&mut self, region: (i32, i32, i32, i32)) -> Option<Vec<u8>> {
        let (x, y, w, h) = region;
        let mgr = self.state.screencopy.clone().expect("screencopy");
        let output = self.state.output.clone().expect("output");
        self.state.frame = FrameState::Waiting;
        let frame = mgr.capture_output_region(0, &output, x, y, w, h, &self.qh, ());
        self.conn.flush().ok();
        while self.state.frame == FrameState::Waiting {
            self.dispatch();
        }
        frame.destroy();
        if self.state.frame == FrameState::Failed {
            return None;
        }
        let cap = self.state.capture.as_ref()?;
        Some(cap.map[..(cap.stride * cap.height) as usize].to_vec())
    }
}

/// XRGB8888 arrives as B, G, R, X; the first channel decides, and every pixel
/// of the probe has to agree so a region straddling the marker's edge is not
/// read as a flip.
fn white(frame: &[u8]) -> bool {
    frame.as_chunks::<4>().0.iter().all(|px| px[0] > 192)
}

fn black(frame: &[u8]) -> bool {
    frame.as_chunks::<4>().0.iter().all(|px| px[0] < 64)
}

/// A whole-output capture is indexed in physical pixels while a region capture
/// is asked for in output-logical ones, so the located marker has to be divided
/// by the output's scale before it can be asked for again. A fractional scale
/// lands the center between logical pixels; rounding keeps it well inside a
/// marker tens of pixels wide.
fn logical_center(bbox: (i32, i32, i32, i32), scale_x: f64, scale_y: f64) -> (i32, i32) {
    let (x0, y0, x1, y1) = bbox;
    let cx = f64::from(x0 + x1) / 2.0 / scale_x;
    let cy = f64::from(y0 + y1) / 2.0 / scale_y;
    (cx.round() as i32, cy.round() as i32)
}

/// The virtual keyboard needs its keymap before any key it sends means anything
/// to the compositor.
fn install_keymap(vk: &zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1) {
    let mut keymap = tempfile::tempfile().expect("keymap file");
    keymap.write_all(KEYMAP.as_bytes()).expect("keymap write");
    keymap.flush().ok();
    vk.keymap(
        wl_keyboard::KeymapFormat::XkbV1 as u32,
        keymap.as_fd(),
        KEYMAP.len() as u32,
    );
}

/// Holds the injected key down for as long as it lives. A panic between the
/// press and the release would otherwise leave the virtual keyboard holding a
/// key for whoever the compositor focuses next.
struct KeyHeld {
    vk: zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    conn: Connection,
    start: Instant,
    key: u32,
    released: bool,
}

impl KeyHeld {
    fn press(
        vk: &zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
        conn: &Connection,
        start: Instant,
        key: u32,
    ) -> Self {
        vk.key(
            start.elapsed().as_millis() as u32,
            key,
            wl_keyboard::KeyState::Pressed as u32,
        );
        conn.flush().ok();
        Self {
            vk: vk.clone(),
            conn: conn.clone(),
            start,
            key,
            released: false,
        }
    }

    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.vk.key(
            self.start.elapsed().as_millis() as u32,
            self.key,
            wl_keyboard::KeyState::Released as u32,
        );
        self.conn.flush().ok();
    }
}

/// One complete keystroke, for the setup phases that do not time anything.
fn tap(
    vk: &zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    conn: &Connection,
    start: Instant,
    key: u32,
) {
    KeyHeld::press(vk, conn, start, key).release();
}

impl Drop for KeyHeld {
    fn drop(&mut self) {
        self.release();
    }
}

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len() as f64
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    let rank = ((p * sorted.len() as f64).ceil() as usize).max(1);
    sorted[rank - 1]
}

/// `niri msg --json focused-window`, the safety condition: a virtual keyboard
/// types into whatever the compositor has focused, never into a pid the tool
/// chose.
fn focused_window() -> Option<(u64, u32)> {
    let out = Command::new("niri")
        .args(["msg", "--json", "focused-window"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some((
        value.get("id")?.as_u64()?,
        u32::try_from(value.get("pid")?.as_u64()?).ok()?,
    ))
}

/// Aborts unless the focused window is the one the calibration mapped: both the
/// id and the pid, since a window of the same process is still the wrong target
/// for a keystroke aimed at this surface.
fn require_focus(expect_id: Option<u64>, expect_pid: u32) -> u64 {
    match focused_window() {
        Some((id, pid)) if pid == expect_pid && expect_id.is_none_or(|want| want == id) => id,
        other => {
            eprintln!(
                "wl-latency: refusing to inject: niri's focused window is {other:?}, not \
                 {} of pid {expect_pid}. A keystroke would land somewhere else.",
                match expect_id {
                    Some(id) => format!("window {id}"),
                    None => "a window".to_string(),
                }
            );
            std::process::exit(2);
        }
    }
}

// ── measurement ──────────────────────────────────────────────────────

/// Typometer's own constants, so a Wayland bar and a macOS bar are the same
/// measurement taken through different APIs (upstream `BenchmarkImpl`).
const PATTERN_LENGTH: usize = 5;
const MIN_LINE_LENGTH: usize = 15;
const PATTERN_INSERTION_DELAY: u64 = 300;
const DELETION_DELAY: u64 = 200;
/// How long a keystroke has to reach the screen before the leg is called
/// broken. A wall-clock bound, not a count of polls: a screencopy round trip
/// is a fraction of a millisecond, so a fixed number of them is a deadline
/// that shortens exactly when the machine is busy enough to need a longer one.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(2);
/// How far apart the two captures `still_capture` compares are: longer than a
/// frame at 60 Hz, so a window that is still animating shows a difference.
const STILL_INTERVAL: Duration = Duration::from_millis(100);
const MIN_STEP: f64 = 4.0;
const MAX_DEVIATION: f64 = 2.0;
/// How many rows of a whole-screen diff are offered to the pattern detector,
/// busiest first. Bounds the scan without bounding what it can find: the
/// pattern's own rows carry more marks than anything a quiet desktop repaints.
const CANDIDATE_ROWS: usize = 64;

/// Two pixels are the same color unless a channel moves by more than this.
/// Antialiasing puts a glyph's own edge well above it and a compositor's
/// dithering well below.
const COLOR_EPSILON: u8 = 24;

/// The reference pattern, once located: where its glyphs sit and what the
/// screen looks like where they do not.
struct Row {
    /// Center of the first glyph, in the logical pixels a region capture takes.
    x0: f64,
    /// Logical pixels from one cell to the next.
    step: f64,
    y: i32,
    background: [u8; 3],
    /// Cells that fit before the line has to be cleared and restarted.
    line: usize,
}

fn rgb(frame: &[u8], stride: usize, x: i32, y: i32) -> [u8; 3] {
    let at = y as usize * stride + x as usize * 4;
    [frame[at], frame[at + 1], frame[at + 2]]
}

fn same(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter()
        .zip(b.iter())
        .all(|(l, r)| l.abs_diff(*r) <= COLOR_EPSILON)
}

/// The evenly spaced run of `PATTERN_LENGTH` marks on one row, if there is one.
///
/// A window of consecutive clusters rather than the first five: the cursor
/// leaves a mark of its own past the pattern, and a row that also crosses
/// something else repainting carries marks that are not the pattern at all.
fn sequence_in(clusters: &[(i32, i32)]) -> Option<(f64, f64)> {
    let center = |(a, b): (i32, i32)| f64::from(a + b) / 2.0;
    for window in clusters.windows(PATTERN_LENGTH) {
        let first = center(window[0]);
        let step = (center(window[PATTERN_LENGTH - 1]) - first) / (PATTERN_LENGTH - 1) as f64;
        if step < MIN_STEP {
            continue;
        }
        if window
            .iter()
            .enumerate()
            .all(|(i, c)| (center(*c) - (first + step * i as f64)).abs() <= MAX_DEVIATION)
        {
            return Some((first, step));
        }
    }
    None
}

/// Where the pattern landed, from the capture taken before it was typed and
/// the one taken after.
///
/// Upstream diffs recognized glyph *areas* and keeps the ones only the second
/// image has; the same exclusion is what matters here and a terminal reaches
/// it more cheaply. The screen under a freshly cleared `cat` is one flat
/// color, so a pixel belongs to the pattern when it departs from that color in
/// the second image — which drops the cursor the first glyph replaced (it is
/// back to background) without dropping the glyph itself.
///
/// Every judgment is made on one row at a time, and rows are tried in the
/// order of how much changed on them. A whole-screen diff is not a quiet
/// picture — the field's previous window is still closing, and the desktop
/// has its own clock — so a single row chosen by a global maximum lands on
/// whatever else repainted. A row is the pattern's only if the pattern's
/// shape is on it.
fn locate(
    before: &[u8],
    after: &[u8],
    stride: usize,
    width: i32,
    height: i32,
) -> Result<Row, String> {
    let mut rows: Vec<(usize, i32, i32, i32)> = Vec::new();
    for y in 0..height {
        let mut span: Option<(i32, i32)> = None;
        let mut marks = 0usize;
        for x in 0..width {
            if !same(rgb(before, stride, x, y), rgb(after, stride, x, y)) {
                marks += 1;
                span = Some(match span {
                    None => (x, x),
                    Some((a, b)) => (a.min(x), b.max(x)),
                });
            }
        }
        if let Some((a, b)) = span {
            rows.push((marks, y, a, b));
        }
    }
    if rows.is_empty() {
        return Err("nothing on screen changed when the pattern was typed".into());
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));

    let mut seen = 0usize;
    for &(_, y, from, to) in rows.iter().take(CANDIDATE_ROWS) {
        // The background is read from the *first* capture and from this row
        // alone: the pattern lands on background, so the color under most of
        // what changed here is the color the glyph has to depart from.
        let mut tally: std::collections::HashMap<[u8; 3], usize> = std::collections::HashMap::new();
        for x in from..=to {
            if !same(rgb(before, stride, x, y), rgb(after, stride, x, y)) {
                *tally.entry(rgb(before, stride, x, y)).or_default() += 1;
            }
        }
        let Some(background) = tally
            .iter()
            .max_by_key(|(_, n)| **n)
            .map(|(color, _)| *color)
        else {
            continue;
        };
        let mut clusters: Vec<(i32, i32)> = Vec::new();
        for x in from..=to {
            if same(rgb(after, stride, x, y), background) {
                continue;
            }
            match clusters.last_mut() {
                Some(last) if last.1 + 1 == x => last.1 = x,
                _ => clusters.push((x, x)),
            }
        }
        seen = seen.max(clusters.len());
        let Some((x0, step)) = sequence_in(&clusters) else {
            continue;
        };
        // How far the line runs, measured from past the pattern so the
        // pattern itself is not read as the end of it.
        let offset = PATTERN_LENGTH + 3;
        let start = (x0 + step * offset as f64).round() as i32;
        let mut x = start.max(0);
        while x < width && same(rgb(after, stride, x, y), background) {
            x += 1;
        }
        let line = ((x - start) as f64 / step).floor() as usize + offset - 1;
        if line < MIN_LINE_LENGTH {
            return Err(format!("only {line} cells fit on the line"));
        }
        return Ok(Row {
            x0,
            step,
            y,
            background,
            line,
        });
    }
    Err(format!(
        "no row carries {PATTERN_LENGTH} evenly spaced marks (the busiest row \
         of {} that changed carries {seen})",
        rows.len()
    ))
}

/// The focused output's logical geometry, and a refusal if it is transformed.
///
/// A region capture is asked for in output-logical coordinates while a
/// whole-output capture comes back in image space, and under a rotation the
/// two do not even share an axis. Rather than carry a transform through every
/// coordinate, the instrument declines the case: the field runs on an
/// untransformed output, and a rotated one would be a different measurement.
fn focused_output() -> Result<(i32, i32, f64), String> {
    let out = Command::new("niri")
        .args(["msg", "--json", "focused-output"])
        .output()
        .map_err(|e| format!("niri msg: {e}"))?;
    if !out.status.success() {
        return Err("niri could not name the focused output".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("niri msg: {e}"))?;
    let logical = value
        .get("logical")
        .ok_or("the output has no logical size")?;
    let transform = logical
        .get("transform")
        .and_then(|t| t.as_str())
        .unwrap_or("");
    if transform != "Normal" {
        return Err(format!(
            "the focused output is {transform}; wl-latency reads a region in \
             output coordinates and cannot follow a transform"
        ));
    }
    let field = |name: &str| -> Result<i64, String> {
        logical
            .get(name)
            .and_then(|v| v.as_i64())
            .ok_or_else(|| format!("the output has no {name}"))
    };
    let scale = logical.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0);
    Ok((field("width")? as i32, field("height")? as i32, scale))
}

/// The window's rectangle in output-logical coordinates, when niri reports
/// one: only a floating window carries a position (a tiled one's is null), and
/// the suite floats every window it measures.
///
/// Detection diffs two captures, and on a whole-output capture any other
/// window that repaints between them (a spinner, a clock, a system monitor)
/// competes with the pattern for the busiest row; a text row there passes the
/// evenly-spaced test and the leg fails as a too-short line or a block cursor.
fn window_rect(window_id: u64, out_w: i32, out_h: i32) -> Option<(i32, i32, i32, i32)> {
    let out = Command::new("niri")
        .args(["msg", "--json", "windows"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let windows: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let layout = windows
        .as_array()?
        .iter()
        .find(|w| w.get("id").and_then(|v| v.as_u64()) == Some(window_id))?
        .get("layout")?;
    let pair = |key: &str| -> Option<(f64, f64)> {
        let v = layout.get(key)?.as_array()?;
        Some((v.first()?.as_f64()?, v.get(1)?.as_f64()?))
    };
    let (tx, ty) = pair("tile_pos_in_workspace_view")?;
    let (ox, oy) = pair("window_offset_in_tile").unwrap_or((0.0, 0.0));
    let (w, h) = pair("window_size")?;
    let x0 = ((tx + ox).round() as i32).clamp(0, out_w);
    let y0 = ((ty + oy).round() as i32).clamp(0, out_h);
    let x1 = ((tx + ox + w).round() as i32).clamp(0, out_w);
    let y1 = ((ty + oy + h).round() as i32).clamp(0, out_h);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

/// Aborts unless niri's focused window is the one the suite named.
///
/// The pid check `require_focus` makes cannot be reused: the window under
/// measurement belongs to a terminal the suite launched, whose pid the tool
/// was never told, and whose process tree the suite already resolved to this
/// one id.
fn require_focus_id(window_id: u64) {
    match focused_window() {
        Some((id, _)) if id == window_id => {}
        other => {
            eprintln!(
                "wl-latency: refusing to inject: niri's focused window is {other:?}, not \
                 window {window_id}. A keystroke would land somewhere else."
            );
            std::process::exit(2);
        }
    }
}

/// What one measured leg produced: the samples, and how often a cell was
/// found covered before its key went down and had to be waited out.
struct Measured {
    samples: Vec<f64>,
    covered: usize,
}

/// Captures `region` until two captures `STILL_INTERVAL` apart agree.
///
/// The pattern is found by diffing the capture before it was typed against
/// the one after, so a window still settling from its pin (a resize or move
/// animation, a frame still arriving) puts its own motion into the diff and
/// can lock the detector onto a row one pixel off the glyph.
fn still_capture(inst: &mut Instrument, region: (i32, i32, i32, i32)) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let mut last = inst.capture(region).ok_or("the output would not capture")?;
    loop {
        std::thread::sleep(STILL_INTERVAL);
        let next = inst.capture(region).ok_or("the output would not capture")?;
        if next == last {
            return Ok(next);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the window kept changing for {} ms before the pattern was typed, so a \
                 diff could not tell the pattern from the change",
                ANSWER_TIMEOUT.as_millis()
            ));
        }
        last = next;
    }
}

/// Types `count` characters into the focused window, timing each one from the
/// key going down to the glyph reaching the screen.
///
/// Upstream's synchronous loop, kept step for step except where the key is
/// released: check that the sampled pixel is background, press, poll until it
/// is not, wait out the inter-character delay, move one cell right. Where the line runs out the
/// characters are deleted and the next batch starts from the same point, so
/// the measurement never depends on how the terminal wraps.
///
/// Where upstream reads the screen once at a fixed moment, this polls until
/// `ANSWER_TIMEOUT`: the pattern, the cleared line and the empty cell before a
/// key are each a state the screen reaches, and a terminal that presents a
/// frame late or incomplete (ghostty tip under NVIDIA shows a partly drawn
/// frame and completes it only on a redraw 0.6-1.8 s later) otherwise fails as
/// an undetected pattern or a block cursor it does not have. A state the
/// screen never reaches still fails the leg.
fn measure(
    inst: &mut Instrument,
    vk: &zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    window_id: u64,
    count: usize,
    delay_ms: u64,
    start: Instant,
) -> Result<Measured, String> {
    let (out_w, out_h, scale) = focused_output()?;
    require_focus_id(window_id);

    eprintln!("  wl-latency: inserting a reference pattern...");
    let placed = window_rect(window_id, out_w, out_h);
    let region = placed.unwrap_or((0, 0, out_w, out_h));
    // A whole-output capture holds every other window's repaints, so it is
    // never still; only a located window can be asked to be.
    let before = if placed.is_some() {
        still_capture(inst, region)?
    } else {
        inst.capture(region).ok_or("the output would not capture")?
    };
    let (stride, cap_w, cap_h) = {
        let cap = inst.state.capture.as_ref().ok_or("no capture")?;
        (cap.stride as usize, cap.width as i32, cap.height as i32)
    };
    for _ in 0..PATTERN_LENGTH {
        tap(vk, &inst.conn, start, KEY_PERIOD);
    }
    std::thread::sleep(Duration::from_millis(PATTERN_INSERTION_DELAY));

    eprintln!("  wl-latency: detecting screen metrics...");
    let typed = Instant::now();
    let row = loop {
        let after = inst.capture(region).ok_or("the output would not capture")?;
        match locate(&before, &after, stride, cap_w, cap_h) {
            Ok(row) => break row,
            Err(why) if typed.elapsed() >= ANSWER_TIMEOUT => {
                for _ in 0..PATTERN_LENGTH {
                    tap(vk, &inst.conn, start, KEY_BACKSPACE);
                }
                return Err(format!(
                    "{why}, {} ms after the pattern was typed",
                    (typed.elapsed().as_millis() + u128::from(PATTERN_INSERTION_DELAY))
                ));
            }
            Err(_) => {}
        }
    };
    let found_after = typed.elapsed().as_millis() + u128::from(PATTERN_INSERTION_DELAY);
    // The capture is indexed in physical pixels and a region is asked for in
    // logical ones; on a fractional scale the rounding lands well inside a
    // glyph the detector already found several pixels wide.
    let (rx, ry, rw, rh) = region;
    let to_logical = |v: f64| v * f64::from(cap_w).recip() * f64::from(rw);
    let x0 = f64::from(rx) + to_logical(row.x0);
    let step = to_logical(row.step);
    let y = ry + (f64::from(row.y) / f64::from(cap_h) * f64::from(rh)).round() as i32;
    eprintln!(
        "  wl-latency: pattern on row {y}, cells {step:.2} px apart from x {x0:.1}, \
         {} usable (output {out_w}x{out_h} at scale {scale}), found {found_after} ms \
         after typing",
        row.line
    );

    let probe = |x: f64| (x.round() as i32, y, 1, 1);
    let sample_is_background = |inst: &mut Instrument, x: f64| -> Result<bool, String> {
        let frame = inst
            .capture(probe(x))
            .ok_or("a screencopy frame failed while sampling")?;
        Ok(same([frame[0], frame[1], frame[2]], row.background))
    };
    let reaches = |inst: &mut Instrument, x: f64, background: bool| -> Result<bool, String> {
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        while Instant::now() < deadline {
            if sample_is_background(inst, x)? == background {
                return Ok(true);
            }
        }
        Ok(false)
    };
    // Named in every failure that reads a pixel, because the two causes that
    // look alike from the pixel alone — a terminal that never redrew and a
    // window that is no longer where it was located — need different fixes.
    let describe = |inst: &mut Instrument, x: f64| -> String {
        let seen = inst
            .capture(probe(x))
            .map(|f| format!("{:?}", [f[2], f[1], f[0]]))
            .unwrap_or_else(|| "nothing".into());
        let bg = row.background;
        let moved = match (placed, window_rect(window_id, out_w, out_h)) {
            (Some(was), Some(now)) if was != now => {
                format!("; the window moved from {was:?} to {now:?}")
            }
            _ => String::new(),
        };
        format!(
            "pixel ({}, {y}) reads RGB {seen}, background is {:?}{moved}",
            x.round(),
            [bg[2], bg[1], bg[0]]
        )
    };
    if !reaches(inst, x0, false)? {
        let why = describe(inst, x0);
        return Err(format!(
            "the located glyph reads as background where it was found: {why}"
        ));
    }

    let delete = |inst: &mut Instrument, n: usize, x: f64| -> Result<(), String> {
        // A second round of backspaces cannot erase anything the first did
        // not mean to: `cat` reads in canonical mode, where an erase at the
        // start of the line is ignored, so it only recovers a lost key.
        for _ in 0..2 {
            for _ in 0..n {
                tap(vk, &inst.conn, start, KEY_BACKSPACE);
            }
            if reaches(inst, x, true)? {
                std::thread::sleep(Duration::from_millis(DELETION_DELAY));
                return Ok(());
            }
        }
        let why = describe(inst, x);
        Err(format!("the line would not clear: {why}"))
    };
    eprintln!("  wl-latency: deleting the pattern...");
    delete(inst, PATTERN_LENGTH, x0)?;

    let mut samples: Vec<f64> = Vec::with_capacity(count);
    let mut covered = 0usize;
    while samples.len() < count {
        let batch = row.line.min(count - samples.len());
        for i in 0..batch {
            let x = x0 + step * i as f64;
            if !sample_is_background(inst, x)? {
                covered += 1;
                if !reaches(inst, x, true)? {
                    let why = describe(inst, x);
                    return Err(format!(
                        "previously undetected block cursor found: the sampled pixel \
                         stayed covered for {} ms before the key was pressed ({why})",
                        ANSWER_TIMEOUT.as_millis()
                    ));
                }
            }
            require_focus_id(window_id);
            let t0 = Instant::now();
            // Released at once rather than held until the glyph shows, as
            // upstream does: a Wayland client repeats a held key itself, so a
            // glyph slower than the seat's repeat delay would type a second
            // one into the next cell before this one was even seen.
            tap(vk, &inst.conn, start, KEY_PERIOD);
            let mut hit = None;
            let deadline = t0 + ANSWER_TIMEOUT;
            while Instant::now() < deadline {
                if !sample_is_background(inst, x)? {
                    hit = Some(t0.elapsed());
                    break;
                }
            }
            let Some(d) = hit else {
                return Err(format!(
                    "no glyph reached the screen for character {} of {count}",
                    samples.len() + 1
                ));
            };
            samples.push(d.as_secs_f64() * 1000.0);
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        delete(inst, batch, x0)?;
    }
    Ok(Measured { samples, covered })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| -> Option<String> {
        let at = args.iter().position(|a| a == name)?;
        args.get(at + 1).cloned()
    };
    let count: usize = flag("--count").and_then(|v| v.parse().ok()).unwrap_or(300);
    let delay_ms: u64 = flag("--delay").and_then(|v| v.parse().ok()).unwrap_or(150);
    let json_out = flag("--json");

    let window_id = flag("--window-id").and_then(|v| v.parse::<u64>().ok());
    let mode = ["--globals", "--calibrate", "--measure"]
        .into_iter()
        .find(|m| args.iter().any(|a| a == m));
    if mode.is_none() {
        eprintln!(
            "usage: wl-latency --globals\n       wl-latency --calibrate [--count N] \
             [--delay MS] [--json PATH]\n       wl-latency --measure --window-id ID \
             [--count N] [--delay MS] [--json PATH]"
        );
        std::process::exit(64);
    }

    let conn = Connection::connect_to_env().expect("no Wayland display");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let display = conn.display();
    display.get_registry(&qh, ());
    let mut state = State::new();
    queue.roundtrip(&mut state).expect("registry roundtrip");

    let mut inst = Instrument {
        conn,
        queue,
        qh,
        state,
    };

    if args.iter().any(|a| a == "--globals") {
        inst.state.globals.sort();
        for (interface, version) in &inst.state.globals {
            println!("{interface} v{version}");
        }
        return;
    }

    for (label, present) in [
        ("wl_compositor", inst.state.compositor.is_some()),
        ("wl_shm", inst.state.shm.is_some()),
        ("xdg_wm_base", inst.state.wm_base.is_some()),
        ("wl_seat", inst.state.seat.is_some()),
        (
            "zwp_virtual_keyboard_manager_v1",
            inst.state.vk_manager.is_some(),
        ),
        (
            "zwlr_screencopy_manager_v1",
            inst.state.screencopy.is_some(),
        ),
        ("wl_output", inst.state.output.is_some()),
    ] {
        if !present {
            eprintln!("wl-latency: compositor does not offer {label}");
            std::process::exit(3);
        }
    }

    let qh = inst.qh.clone();
    if mode == Some("--measure") {
        let Some(window_id) = window_id else {
            eprintln!("wl-latency: --measure needs --window-id (niri's window id)");
            std::process::exit(64);
        };
        let seat = inst.state.seat.clone().unwrap();
        let vk = inst
            .state
            .vk_manager
            .as_ref()
            .unwrap()
            .create_virtual_keyboard(&seat, &qh, ());
        install_keymap(&vk);
        inst.roundtrip();
        let start = Instant::now();
        let Measured { samples, covered } =
            match measure(&mut inst, &vk, window_id, count, delay_ms, start) {
                Ok(measured) => measured,
                Err(why) => {
                    eprintln!("wl-latency: {why}");
                    std::process::exit(4);
                }
            };
        if covered > 0 {
            eprintln!(
                "  wl-latency: {covered} of {} cells were covered before their key and \
                 cleared on their own; the terminal presented a frame it later replaced",
                samples.len()
            );
        }
        let mut sorted = samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        eprintln!(
            "  wl-latency: {} samples, min {:.1} mean {:.1} max {:.1} p95 {:.1} ms",
            sorted.len(),
            sorted[0],
            mean(&samples),
            sorted[sorted.len() - 1],
            pct(&sorted, 0.95),
        );
        if let Some(path) = json_out {
            let values = samples
                .iter()
                .map(|s| format!("{s:.3}"))
                .collect::<Vec<_>>()
                .join(", ");
            let json = format!(
                "{{\n  \"instrument\": \"wl-latency\",\n  \"count\": {count},\n  \
                 \"delay_ms\": {delay_ms},\n  \"covered_before_key\": {covered},\n  \
                 \"samples_ms\": [{values}]\n}}\n"
            );
            std::fs::write(path, json).expect("write json");
        }
        return;
    }
    let compositor = inst.state.compositor.clone().unwrap();
    let surface = compositor.create_surface(&qh, ());
    let xdg = inst
        .state
        .wm_base
        .as_ref()
        .unwrap()
        .get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("wl-latency".into());
    toplevel.set_app_id(APP_ID.into());
    toplevel.set_fullscreen(None);
    surface.commit();
    inst.state.surface = Some(surface.clone());
    while !inst.state.configured {
        inst.dispatch();
    }
    let (w, h) = inst.state.size;
    let (w, h) = if w > 0 && h > 0 { (w, h) } else { (640, 480) };
    let marker = (w / 4, h / 4, 48);
    make_surface_buffers(&mut inst.state, &qh, w, h, marker).expect("buffers");
    let dark = inst.state.black.clone().unwrap();
    surface.attach(Some(&dark), 0, 0);
    surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
    surface.commit();
    inst.roundtrip();

    let seat = inst.state.seat.clone().unwrap();
    let vk = inst
        .state
        .vk_manager
        .as_ref()
        .unwrap()
        .create_virtual_keyboard(&seat, &qh, ());
    install_keymap(&vk);
    inst.roundtrip();

    let pid = std::process::id();
    std::thread::sleep(Duration::from_millis(400));
    inst.roundtrip();
    let window_id = require_focus(None, pid);
    eprintln!("wl-latency: calibrating in niri window {window_id} (pid {pid}), surface {w}x{h}");

    // Locate the marker the way the terminal path locates its glyph: capture
    // the whole output twice and diff, so the pixel that is timed is one the
    // capture path has actually been shown to see.
    let full = (0, 0, w, h);
    let before = inst.capture(full).expect("capture");
    surface.attach(inst.state.white.as_ref(), 0, 0);
    surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
    surface.commit();
    inst.conn.flush().ok();
    std::thread::sleep(Duration::from_millis(300));
    let after = inst.capture(full).expect("capture");
    let cap = inst.state.capture.as_ref().unwrap();
    let stride = cap.stride as usize;
    let (cap_w, cap_h) = (cap.width as i32, cap.height as i32);
    // The surface's buffers carry no `set_buffer_scale`, so the compositor
    // upscales them on a HiDPI output: the marker stays where it was asked for
    // in logical pixels and the capture shows it scaled.
    let scale_x = f64::from(cap_w) / f64::from(w);
    let scale_y = f64::from(cap_h) / f64::from(h);
    let mut bbox: Option<(i32, i32, i32, i32)> = None;
    for y in 0..cap_h {
        for x in 0..cap_w {
            let at = y as usize * stride + x as usize * 4;
            if before[at].abs_diff(after[at]) > 64 {
                bbox = Some(match bbox {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
        }
    }
    let Some(bbox) = bbox else {
        eprintln!("wl-latency: the marker never appeared in a screencopy frame");
        std::process::exit(4);
    };
    let (cx, cy) = logical_center(bbox, scale_x, scale_y);
    eprintln!(
        "wl-latency: marker spans {bbox:?} in the captured image ({cap_w}x{cap_h}, scale \
         {scale_x}x{scale_y}), painted at {},{}",
        marker.0, marker.1
    );
    // A whole-output capture is indexed in image space, but a region capture is
    // asked for in output space, and the two differ by the output's transform.
    // Rather than reimplement the transform, ask for each candidate region while
    // the marker is still white and keep the one that sees it.
    let mut probe = None;
    for (x, y) in [
        (cx, cy),
        (cx, h - 1 - cy),
        (w - 1 - cx, cy),
        (w - 1 - cx, h - 1 - cy),
    ] {
        let region = (x - 4, y - 4, 8, 8);
        if region.0 < 0 || region.1 < 0 || region.0 + 8 > w || region.1 + 8 > h {
            continue;
        }
        if inst.capture(region).is_some_and(|f| white(&f)) {
            probe = Some(region);
            break;
        }
    }
    let Some(probe) = probe else {
        eprintln!("wl-latency: no region capture sees the marker the whole-output capture found");
        std::process::exit(4);
    };
    eprintln!("wl-latency: probing output region {probe:?}");

    // The polling resolution: how long one screencopy round trip takes when
    // the screen is not changing under it. Every sample is quantized by it.
    let mut idle: Vec<f64> = Vec::with_capacity(30);
    for _ in 0..30 {
        let at = Instant::now();
        inst.capture(probe).expect("capture");
        idle.push(at.elapsed().as_secs_f64() * 1000.0);
    }
    idle.sort_by(|a, b| a.partial_cmp(b).unwrap());
    eprintln!(
        "idle screencopy round trip: p50={:.2} ms p95={:.2} ms",
        pct(&idle, 0.50),
        pct(&idle, 0.95)
    );

    let mut samples: Vec<f64> = Vec::with_capacity(count);
    let mut handled: Vec<f64> = Vec::with_capacity(count);
    let start = Instant::now();
    let mut timeouts = 0u32;
    for _ in 0..count {
        surface.attach(inst.state.black.as_ref(), 0, 0);
        surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        surface.commit();
        inst.conn.flush().ok();
        // Steady state before the key: the frame that still carries the last
        // flip would otherwise be read as an instant answer.
        let mut settled = false;
        for _ in 0..120 {
            let frame = inst.capture(probe).expect("capture");
            if black(&frame) {
                settled = true;
                break;
            }
        }
        if !settled {
            eprintln!("wl-latency: the marker never returned to black");
            std::process::exit(5);
        }
        std::thread::sleep(Duration::from_millis(delay_ms));
        require_focus(Some(window_id), pid);
        let flips = inst.state.flips;
        inst.state.flip_at = None;
        let t0 = Instant::now();
        let mut held = KeyHeld::press(&vk, &inst.conn, start, KEY_A);
        let mut hit = None;
        let mut lost = false;
        for _ in 0..400 {
            match inst.capture(probe) {
                Some(frame) if white(&frame) => {
                    hit = Some(t0.elapsed());
                    break;
                }
                Some(_) => {}
                None => {
                    lost = true;
                    break;
                }
            }
        }
        held.release();
        if lost {
            eprintln!("wl-latency: a screencopy frame failed while a key was pending");
            std::process::exit(8);
        }
        if let Some(at) = inst.state.flip_at {
            handled.push((at - t0).as_secs_f64() * 1000.0);
        }
        match hit {
            Some(d) => samples.push(d.as_secs_f64() * 1000.0),
            None => {
                timeouts += 1;
                eprintln!(
                    "wl-latency: no flip observed for one injection (focus={} flips={} waited={:?})",
                    inst.state.focused,
                    inst.state.flips,
                    t0.elapsed()
                );
            }
        }
        if inst.state.flips == flips {
            eprintln!("wl-latency: the surface never saw the key it was sent");
            std::process::exit(6);
        }
    }

    if samples.is_empty() {
        eprintln!("wl-latency: no sample completed");
        std::process::exit(7);
    }
    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p5 = pct(&sorted, 0.05);
    let p50 = pct(&sorted, 0.50);
    let p95 = pct(&sorted, 0.95);
    eprintln!(
        "samples={} timeouts={timeouts} mean={:.2} p5={p5:.2} p50={p50:.2} p95={p95:.2} \
         p95-p5={:.2} min={:.2} max={:.2}",
        samples.len(),
        mean(&samples),
        p95 - p5,
        sorted[0],
        sorted[sorted.len() - 1],
    );
    let mut hsorted = handled.clone();
    hsorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if !hsorted.is_empty() {
        eprintln!(
            "inject->key-event: n={} mean={:.2} p5={:.2} p50={:.2} p95={:.2}",
            hsorted.len(),
            mean(&handled),
            pct(&hsorted, 0.05),
            pct(&hsorted, 0.50),
            pct(&hsorted, 0.95),
        );
    }
    if let Some(path) = json_out {
        let series = |v: &[f64]| {
            v.iter()
                .map(|s| format!("{s:.3}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let json = format!(
            "{{\n  \"instrument\": \"wl-latency\",\n  \"samples_ms\": [{}],\n  \"inject_to_key_event_ms\": [{}],\n  \"idle_capture_ms\": [{}]\n}}\n",
            series(&samples),
            series(&handled),
            series(&idle)
        );
        std::fs::write(path, json).expect("write json");
    }
}

#[cfg(test)]
mod tests {
    use super::{logical_center, sequence_in};

    /// Five 3-px marks whose centers sit `step` apart, starting at `first`.
    fn marks(first: i32, step: i32, count: usize) -> Vec<(i32, i32)> {
        (0..count)
            .map(|i| {
                let center = first + step * i as i32;
                (center - 1, center + 1)
            })
            .collect()
    }

    #[test]
    fn an_evenly_spaced_run_gives_its_first_center_and_its_step() {
        let found = sequence_in(&marks(100, 11, 5)).expect("a run of five");
        assert_eq!(found, (100.0, 11.0));
    }

    #[test]
    fn the_cursor_mark_past_the_pattern_is_not_taken_for_a_glyph() {
        // The bar the cursor left in the cell after the last `.` sits one
        // step further along, so a window of five starting at it would need
        // marks that are not there.
        let mut clusters = marks(100, 11, 5);
        clusters.push((155, 156));
        assert_eq!(sequence_in(&clusters), Some((100.0, 11.0)));
    }

    #[test]
    fn marks_that_something_else_repainted_do_not_make_a_pattern() {
        // Five marks at wildly uneven spacing: a row crossing another
        // window's redraw, which is what a whole-screen diff picks up.
        let clusters = vec![(10, 12), (40, 42), (44, 46), (200, 260), (300, 302)];
        assert_eq!(sequence_in(&clusters), None);
    }

    #[test]
    fn a_run_embedded_in_noise_is_still_found() {
        let mut clusters = vec![(0, 4)];
        clusters.extend(marks(100, 11, 5));
        clusters.push((400, 402));
        assert_eq!(sequence_in(&clusters), Some((100.0, 11.0)));
    }

    #[test]
    fn cells_closer_than_the_minimum_step_are_not_a_pattern() {
        // Below `MIN_STEP` the "marks" are the sub-strokes of one glyph, not
        // one glyph per cell.
        assert_eq!(sequence_in(&marks(100, 3, 5)), None);
    }

    #[test]
    fn fewer_marks_than_the_pattern_is_never_a_match() {
        assert_eq!(sequence_in(&marks(100, 11, 4)), None);
    }

    #[test]
    fn logical_center_divides_the_located_marker_by_the_output_scale() {
        // A 48-logical-pixel marker at logical (480, 270), captured at each
        // scale: the center comes back in logical pixels either way.
        assert_eq!(logical_center((480, 270, 527, 317), 1.0, 1.0), (504, 294));
        assert_eq!(logical_center((960, 540, 1055, 635), 2.0, 2.0), (504, 294));
        assert_eq!(logical_center((720, 405, 791, 476), 1.5, 1.5), (504, 294));
    }

    #[test]
    fn logical_center_scales_each_axis_on_its_own() {
        assert_eq!(logical_center((960, 270, 1055, 317), 2.0, 1.0), (504, 294));
    }
}

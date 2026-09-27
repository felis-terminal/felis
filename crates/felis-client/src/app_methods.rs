//! `App` inherent methods.

use super::{
    Action, ActivationTarget, App, AppEvent, CarrierReader, CarrierWriter, ClipboardScope,
    ConnToDaemonMsg, Correlation, DEFAULT_FONT_SIZE_LOGICAL_PX, DragState, FrameReader,
    FrameWriter, GridPos, ImageShadow, Ime, InputMsg, Instant, IpcAction, Key, Keymap, Landing,
    ModifiersState, MouseAction, MouseButton, MouseEvent, MouseState, NamedKey, Offer,
    OutgoingFrame, PhysicalPosition, PhysicalSize, PipeState, Pumps, Reconnector,
    RegionToDaemonMsg, Renderer, SearchDirection, SearchHitRecord, SearchHitSpan, SearchOverlay,
    SearchToDaemonMsg, SearchUi, SearchUiMode, Selection, SessionInfo, SessionToDaemonMsg,
    SharedDriver, SwitchState, TrailRecord, TrailState, activation_target, apply_window_backdrop,
    codec, compose_window_title, composed_row_for_line, config, configured_font_size_logical_px,
    cursor_trail, dial_and_land, error, font_features_differ, font_settings_differ,
    font_stack_inputs_differ, ime_cursor_area_pixels, info, keymap_adapter, launch_args,
    pointer_icon, preedit_from_ime, renderer_config_from, reported_theme_differs,
    resolve_local_socket, viewport_for_hit, warn,
};
use crate::winit_keys;
use felis_client_core::confirm::{ConfirmDecision, PendingConfirm, decide_confirm_key};
use felis_client_core::connector::{Carrier, RemoteSpawn, connect_carrier};
use felis_client_core::pipe::write_region_file;
use felis_client_core::roster::RingKey;
use felis_client_core::{FontSizeStep, PipeRegionSource, PipeTarget};
use felis_protocol::messages::{GridDims, RegionPosition, RequestedDims};

impl App {
    /// `current_font_size_logical_px` (the unit `font.size_px` and the zoom
    /// contract are defined in) times the window's scale factor.
    pub(crate) fn font_size_physical_px(&self) -> f32 {
        let scaled = f64::from(self.current_font_size_logical_px) * self.scale_factor;
        scaled as f32
    }

    /// Resizes the shadow locally so the renderer sees the new dimensions
    /// before the daemon's burst lands.
    pub(crate) fn reflow(&mut self, size: PhysicalSize<u32>) {
        let Some(surface) = self.surface.as_ref() else {
            return;
        };
        let metrics = surface.renderer.cell_metrics();
        let cw = metrics.width.max(1);
        let ch = metrics.height.max(1);
        let cols = (size.width / cw).clamp(1, u16::MAX as u32) as u16;
        let rows = (size.height / ch).clamp(1, u16::MAX as u32) as u16;
        // A selection is anchored in viewport coordinates and the daemon
        // reflows the grid under it (REQ-604). Only on a real dims change:
        // a same-dims reflow must not clear a selection mid-drag.
        let dims_changed = {
            let grid = self.shadow.screen();
            rows != grid.rows() || cols != grid.cols()
        };
        if dims_changed {
            self.drag = DragState::Idle;
            self.update_renderer_selection();
        }
        self.shadow.local_resize(rows, cols);
        let resize = InputMsg::Resize {
            dims: RequestedDims {
                rows: u32::from(rows),
                cols: u32::from(cols),
                pixel_w: size.width,
                pixel_h: size.height,
            },
        };
        // Debug-level because resize storms are bursty; the line splits a
        // "program didn't follow the resize" report into client-didn't-send
        // vs daemon-didn't-apply.
        tracing::debug!(
            rows,
            cols,
            pixel_w = size.width,
            pixel_h = size.height,
            cell_w = cw,
            cell_h = ch,
            "reflow → InputMsg::Resize"
        );
        self.send_input(&resize);
        self.resync_pointer_cell();
    }

    /// Re-derives the hovered cell from the last reported pointer position.
    /// Layout changes (font zoom, DPI changes, letterbox re-origins) shift
    /// cells under a stationary pointer without winit motion events, which
    /// would otherwise leave stale cells and hyperlink previews cached.
    fn resync_pointer_cell(&mut self) {
        let Some(position) = self.pointer_px else {
            return;
        };
        self.cursor_cell = self.position_to_cell(position);
        self.cursor_px = self.position_to_pixel(position);
    }

    /// Without the IME re-anchor the popup would lag the new layout until
    /// the daemon's next `CursorState`.
    pub(crate) fn after_metric_change(&mut self, size: PhysicalSize<u32>) {
        self.reflow(size);
        self.update_ime_cursor_area();
    }

    /// `anchor_ime` selects `after_metric_change` vs the bare `reflow` a
    /// switch landing wants mid-handshake, before there is a fresh cursor
    /// to anchor to.
    pub(crate) fn reflow_to_window(&mut self, anchor_ime: bool) {
        let Some(size) = self.surface.as_ref().map(|s| s.window.inner_size()) else {
            return;
        };
        if anchor_ime {
            self.after_metric_change(size);
        } else {
            self.reflow(size);
        }
    }

    /// Pushes the visible state immediately rather than waiting for the
    /// next `about_to_wait` tick.
    pub(crate) fn restart_blink(&mut self) {
        self.cursor_blink.reset(Instant::now());
        if let Some(surface) = self.surface.as_mut() {
            surface.renderer.set_cursor_blink_visible(true);
        }
    }

    /// Without this the platform IME anchors its pre-edit panel at the
    /// window's top-left.
    pub(crate) fn update_ime_cursor_area(&self) {
        let Some(surface) = self.surface.as_ref() else {
            return;
        };
        let (window, renderer) = (&surface.window, &surface.renderer);
        let metrics = renderer.cell_metrics();
        let cursor = self.shadow.screen().cursor();
        let (x, y, w, h) =
            ime_cursor_area_pixels(cursor.col, cursor.row, metrics.width, metrics.height);
        // Shift through the letterbox origin (same-user mirroring,
        // docs/explanation/architecture/session-lifecycle.md).
        let (ox, oy) = renderer.content_origin_px();
        window.set_ime_cursor_area(
            PhysicalPosition::new(f64::from(x) + f64::from(ox), f64::from(y) + f64::from(oy)),
            PhysicalSize::new(w, h),
        );
    }

    pub(crate) fn update_mouse_cursor_icon(&mut self) {
        let new_hover = self.hover_link_target();
        // `apply_pointer_icon` takes effect the moment `set_cursor`
        // returns; the wgpu-rendered preview bar only paints from
        // `on_redraw`, so a changed target has to ask for one.
        if self.hover_target != new_hover {
            self.redraw.request();
        }
        self.hover_target = new_hover;
        self.apply_pointer_icon();
    }

    /// Re-reads the hover target immediately before painting. The bottom
    /// row changes ownership without pointer motion (dismissed search bars,
    /// committed compositions); re-evaluating here prevents stale hover
    /// gestures without scheduling redundant redraws.
    pub(crate) fn settle_hover_target_for_frame(&mut self) {
        // Dimensions can change under the pointer without local reflow
        // (authoritative `GridMsg::Size` from daemons or resized mirrors),
        // arriving as grid messages rather than window events.
        self.resync_pointer_cell();
        let new_hover = self.hover_link_target();
        if self.hover_target == new_hover {
            return;
        }
        self.hover_target = new_hover;
        self.apply_pointer_icon();
    }

    fn apply_pointer_icon(&mut self) {
        let icon = pointer_icon(&self.shadow, self.hover_target.is_some());
        // winit's Wayland backend dedupes nothing: every `set_cursor`
        // re-resolves the icon against the cursor theme and issues a
        // `wl_pointer.set_cursor` plus a surface attach/damage/commit. An
        // applied grid frame calls through here, and a `cat` of a large
        // file lands hundreds a second.
        if self.cursor_icon == Some(icon) {
            return;
        }
        let Some(surface) = self.surface.as_ref() else {
            return;
        };
        surface.window.set_cursor(icon);
        self.cursor_icon = Some(icon);
    }

    /// The activation the pointer is over, if any: the one reading the
    /// Pointer icon, the preview bar, and Ctrl+Click all share
    /// (`hyperlink::activation_target`).
    fn hover_link_target(&self) -> Option<ActivationTarget> {
        activation_target(
            &self.shadow,
            self.cursor_cell,
            self.modifiers,
            self.link_preview_row_free(),
        )
    }

    /// Whether the bottom row would go to a link preview, resolved with
    /// the renderer's own precedence rule rather than a second copy of it
    /// (`felis_render_wgpu::instances::bottom_bar_claim`). Asked with
    /// `link_preview_active = true` because the caller is deciding whether
    /// a preview may exist at all.
    pub(crate) fn link_preview_row_free(&self) -> bool {
        let search_active =
            self.search.mode != SearchUiMode::Off && !self.format_search_label().is_empty();
        let confirm_active = self
            .pending_confirm
            .as_ref()
            .is_some_and(|p| !p.label().is_empty());
        let preedit_active = self
            .surface
            .as_ref()
            .is_some_and(|s| s.renderer.preedit_active());
        felis_render_wgpu::instances::bottom_bar_claim(
            felis_render_wgpu::instances::ChromeBar::from_flags(search_active, confirm_active),
            preedit_active,
            true,
        )
        .draw_link_preview()
    }

    /// Clipped to a width that leaves room for the grid's own columns;
    /// the renderer clips again at the actual column count, so this cap
    /// only bounds how many glyphs a hidden/narrow window rasterizes.
    pub(crate) fn build_link_preview_overlay(
        &self,
    ) -> Option<felis_render_wgpu::LinkPreviewOverlay> {
        const PREVIEW_MAX_CHARS: usize = 120;
        self.hover_target
            .as_ref()
            .map(|target| felis_render_wgpu::LinkPreviewOverlay {
                text: target.preview(PREVIEW_MAX_CHARS),
            })
    }

    pub(crate) fn apply_ime_event(&mut self, ime: &Ime) {
        let cursor = self.shadow.screen().cursor();
        let new = preedit_from_ime(
            ime,
            GridPos {
                row: cursor.row,
                col: cursor.col,
            },
        );
        if let Some(surface) = self.surface.as_mut() {
            surface.renderer.set_preedit(new);
        }
        self.redraw.request();
    }

    pub(crate) const fn selection(&self) -> Option<Selection> {
        match self.drag {
            DragState::Selected(sel) => Some(sel),
            DragState::Idle | DragState::Pending(..) => None,
        }
    }

    pub(crate) fn update_renderer_selection(&mut self) {
        let range = self.selection().map(|sel| {
            let (start, end) = sel.range();
            felis_render_wgpu::SelectionRange {
                start,
                end,
                rectangle: matches!(sel.mode(), felis_client_core::SelectionMode::Rectangle),
            }
        });
        if let Some(surface) = self.surface.as_mut() {
            surface.renderer.set_selection(range);
        }
        self.redraw.request();
    }

    /// Returns the next wake-up the trail needs, or `None` once settled.
    /// Skipped entirely without a post-process shader: with no pass to
    /// read the eased corners this must cost nothing.
    pub(crate) fn tick_cursor_trail(&mut self, now: Instant) -> Option<Instant> {
        let surface = self.surface.as_mut()?;
        let renderer = &mut surface.renderer;
        if !renderer.has_post_shader() {
            return None;
        }
        let grid = self.shadow.screen();
        let cursor = grid.cursor();
        // Same gate the renderer paints the cursor under.
        let target = (cursor.visible
            && self.window_focused
            && self.cursor_blink.visible()
            && self.shadow.viewport() == 0
            && cursor.row < grid.rows()
            && cursor.col < grid.cols())
        .then(|| renderer.cursor_rect_uv(cursor.row, cursor.col, grid.cursor_style()));
        let (width, height) = renderer.size();
        let metrics = renderer.cell_metrics();
        let repaint = self.cursor_trail.update(
            now,
            target,
            cursor_trail::TrailMetrics {
                viewport_px: [width as f32, height as f32],
                cell_px: [metrics.width as f32, metrics.height as f32],
            },
        );
        let geometry = self.cursor_trail.geometry(now);
        renderer.set_trail_state(TrailState {
            cursor_rect: geometry.cursor_rect,
            prev_cursor_rect: geometry.prev_cursor_rect,
            corners_x: geometry.corners_x,
            corners_y: geometry.corners_y,
            seconds_since_change: geometry.seconds_since_change,
        });
        if repaint {
            self.redraw.request();
        }
        self.cursor_trail.next_deadline()
    }

    /// Input-driven by construction: a still pointer produces no events
    /// and therefore no frames.
    pub(crate) fn update_post_mouse_state(&mut self) {
        // The contract's "pointer is not here", as kitty defines it.
        const OUTSIDE: [f32; 2] = [-1.0, -1.0];

        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let renderer = &mut surface.renderer;
        if !renderer.has_post_shader() {
            return;
        }
        let (width, height) = renderer.size();
        let to_uv = |p: PhysicalPosition<f64>| {
            [
                (p.x / f64::from(width.max(1))) as f32,
                (p.y / f64::from(height.max(1))) as f32,
            ]
        };
        renderer.set_mouse_state(MouseState {
            pos_uv: self.pointer_px.map_or(OUTSIDE, to_uv),
            last_press_uv: self.last_press_px.map_or(OUTSIDE, to_uv),
            buttons: [
                f32::from(u8::from(self.held_buttons.is_held(MouseButton::Left))),
                f32::from(u8::from(self.held_buttons.is_held(MouseButton::Right))),
                f32::from(u8::from(self.held_buttons.is_held(MouseButton::Middle))),
                0.0,
            ],
        });
        self.redraw.request();
    }

    /// `None` when the selection holds nothing but whitespace (per-row trim).
    fn selection_text(&self) -> Option<String> {
        let text = self.selection()?.extract_text(self.shadow.screen());
        (!text.is_empty()).then_some(text)
    }

    pub(crate) fn copy_selection_to_clipboard(&self) {
        let Some(text) = self.selection_text() else {
            return;
        };
        // `write_user`, not `write`: the OSC 52 hostile-program gate must
        // not swallow the user's own Ctrl+Shift+C.
        if let Err(err) = self.clipboard.write_user(text.as_bytes()) {
            warn!(?err, "clipboard write");
        } else {
            info!(
                bytes = text.len(),
                "copied selection to clipboard via Ctrl+Shift+C"
            );
        }
    }

    /// Called when the button ending a selection drag is released,
    /// matching xterm / urxvt / alacritty / kitty. A failed PRIMARY write
    /// must not interrupt the gesture.
    pub(crate) fn auto_copy_selection_to_primary(&self) {
        let Some(text) = self.selection_text() else {
            return;
        };
        if let Err(err) = self.clipboard.write_primary(text.as_bytes()) {
            warn!(?err, "PRIMARY write (auto-copy on selection end)");
        }
    }

    /// The `copy { what = "primary" }` binding.
    pub(crate) fn copy_selection_to_primary(&self) {
        let Some(text) = self.selection_text() else {
            return;
        };
        // A no-op `write_primary` is wrong for a chord the user pressed:
        // off Linux this takes the system-clipboard fallback
        // `ClipboardScope::Primary` documents. `write_user` for the same
        // reason Ctrl+Shift+C uses it.
        #[cfg(target_os = "linux")]
        let (written, surface) = (self.clipboard.write_primary(text.as_bytes()), "PRIMARY");
        #[cfg(not(target_os = "linux"))]
        let (written, surface) = (
            self.clipboard.write_user(text.as_bytes()),
            "system clipboard (no PRIMARY on this platform)",
        );
        if let Err(err) = written {
            warn!(?err, surface, "selection copy");
        } else {
            info!(bytes = text.len(), surface, "copied selection");
        }
    }

    /// The one implementation behind the `detach` chord and the
    /// window-close button. The exit rides the proxy rather than
    /// `ActiveEventLoop::exit` because the chord dispatcher has no event
    /// loop to call it on.
    pub(crate) fn request_detach(&self, reason: &'static str) {
        info!(reason, "detaching; the session survives in the daemon");
        self.send_control(&SessionToDaemonMsg::Detach);
        drop(self.proxy.send_event(AppEvent::Detached));
    }

    pub(crate) fn adjust_font_size(&mut self, delta: f32) {
        let new = config::FontConfig::clamped_font_size(self.current_font_size_logical_px, delta);
        if (new - self.current_font_size_logical_px).abs() < f32::EPSILON {
            return;
        }
        self.current_font_size_logical_px = new;
        self.apply_live_font_size("zoom chord");
    }

    /// Ctrl+Shift+0: the browser-style "back to baseline" gesture.
    pub(crate) fn reset_font_size(&mut self) {
        let baseline = self
            .configured_font_size_logical_px
            .unwrap_or(DEFAULT_FONT_SIZE_LOGICAL_PX);
        if (baseline - self.current_font_size_logical_px).abs() < f32::EPSILON {
            return;
        }
        self.current_font_size_logical_px = baseline;
        self.apply_live_font_size("zoom reset");
    }

    /// `Renderer::reload_font_size` (size-only fast path): a held-down zoom
    /// chord must not re-scan the system font directories per keypress.
    pub(crate) fn apply_live_font_size(&mut self, reason: &'static str) {
        let size = self.current_font_size_logical_px;
        let physical = self.font_size_physical_px();
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        surface.renderer.reload_font_size(physical);
        self.reflow_to_window(true);
        self.redraw.request();
        info!(size_px = size, reason, "font size changed");
    }

    /// Ctrl+Shift+R (`principles.md` "3. The daemon owns state, the
    /// client owns pixels"): the shell session, scrollback, and OSC runtime
    /// overrides all survive. The reflow rides the `InputMsg::Resize` path
    /// so the daemon and shadow stay in lockstep.
    pub(crate) fn reload_config(&mut self) {
        let Ok(cfg) = config::EffectiveConfig::try_load_from_source(
            &self.config_source,
            super::GUI_CLIENT_ID,
        ) else {
            // A typo while editing config.toml must not snap the live
            // theme / font / title-prefix back to defaults; the load
            // already logged every diagnostic.
            warn!("config reload failed; keeping previous live state");
            return;
        };
        let new_renderer_cfg = renderer_config_from(&cfg);
        let new_font_size = configured_font_size_logical_px(&cfg);
        let size_changed = new_font_size != self.configured_font_size_logical_px;
        let font_changed =
            size_changed || font_settings_differ(&self.renderer_cfg, &new_renderer_cfg);
        let stack_inputs_changed = font_stack_inputs_differ(&self.renderer_cfg, &new_renderer_cfg);
        let features_changed = font_features_differ(&self.renderer_cfg, &new_renderer_cfg);
        let reported_theme_changed = reported_theme_differs(&self.renderer_cfg, &new_renderer_cfg);
        let post_shader_changed =
            self.renderer_cfg.post_shader_wgsl != new_renderer_cfg.post_shader_wgsl;
        // The next `about_to_wait` arms or drops the animation wake-up off
        // the new policy.
        self.shader_clock.set_mode(cfg.shader.animate);
        if let Some(surface) = self.surface.as_mut() {
            let renderer = &mut surface.renderer;
            renderer.set_theme(&Renderer::theme_from_config(&new_renderer_cfg));
            // Swap only on a real change: rebuilding the pipeline would
            // recompile the shader for an unrelated font edit. A refused
            // shader keeps whatever is loaded.
            if post_shader_changed
                && let Err(err) =
                    renderer.set_post_shader(new_renderer_cfg.post_shader_wgsl.as_deref())
            {
                error!(%err, "post-process shader refused; keeping the previous one");
            }
            if font_changed {
                let logical = new_font_size.unwrap_or(DEFAULT_FONT_SIZE_LOGICAL_PX);
                let physical = (f64::from(logical) * self.scale_factor) as f32;
                if stack_inputs_changed {
                    if let Err(err) = renderer.reload_font(
                        new_renderer_cfg.font_family.as_deref(),
                        Some(physical),
                        &new_renderer_cfg.font_features,
                        &new_renderer_cfg.font_fallbacks,
                        &new_renderer_cfg.style_faces(),
                    ) {
                        warn!(?err, "font hot-reload failed; keeping previous font");
                    }
                } else {
                    // Size first so a combined size+features edit lands at
                    // the new metrics before the features pass resets again.
                    if size_changed {
                        renderer.reload_font_size(physical);
                    }
                    if features_changed {
                        renderer.reload_font_features(&new_renderer_cfg.font_features);
                    }
                }
            }
        }
        self.renderer_cfg = new_renderer_cfg;
        // The daemon answers `OSC 10/11/12 ; ?` from this window's last
        // report and nothing else re-sends it between attaches.
        if reported_theme_changed {
            self.send_configure_theme();
        }
        // A config reload drops any in-flight zoom.
        if font_changed {
            self.configured_font_size_logical_px = new_font_size;
            self.current_font_size_logical_px =
                new_font_size.unwrap_or(DEFAULT_FONT_SIZE_LOGICAL_PX);
        }
        if font_changed {
            self.reflow_to_window(true);
        }
        // `window.opacity` picks the wgpu surface's composite-alpha mode
        // once, at `Renderer::new_with_config` (`select_alpha_mode`); there
        // is no live path to flip an opaque swapchain to a blend-capable
        // one or back. Read before the `title_prefix` move below partially
        // moves `cfg.window`.
        let new_transparent = cfg.window.clamped_opacity() < 1.0;
        if new_transparent != self.transparent {
            warn!(
                "window.opacity crossed the opaque/translucent boundary; restart felis for this to take effect"
            );
        }
        // Gated on `self.transparent` (the surface's live transparency),
        // not `new_transparent`: that is what the macOS effect view can
        // show through until a restart lands the opacity change.
        let new_backdrop = cfg.window.backdrop;
        if new_backdrop != self.backdrop
            && let Some(surface) = self.surface.as_ref()
        {
            self.backdrop = new_backdrop;
            apply_window_backdrop(surface.window.as_ref(), self.backdrop, self.transparent);
        }
        let new_prefix = cfg.window.title_prefix;
        if new_prefix != self.title_prefix {
            self.title_prefix = new_prefix;
            // Not a `set_title` of its own: a reload during a reconnect
            // would drop the disconnected marker this composes in.
            self.refresh_window_title();
        }
        // The platform may flash / resize the window as the OS chrome
        // appears or disappears; unavoidable.
        let new_decorations = cfg.window.decorations;
        if new_decorations != self.decorations
            && let Some(surface) = self.surface.as_ref()
        {
            self.decorations = new_decorations;
            surface.window.set_decorations(new_decorations);
        }
        // `reconfigure` restarts the clock solid-on so a toggle shows
        // immediately; the next `about_to_wait` re-arms off the new policy.
        if cfg.cursor != self.cursor_cfg {
            self.cursor_cfg = cfg.cursor.clone();
            self.cursor_blink.reconfigure(
                self.cursor_cfg.blink,
                self.cursor_cfg.blink_interval_ms,
                Instant::now(),
            );
            self.restart_blink();
        }
        self.scroll_multiplier = cfg.mouse.clamped_scroll_multiplier();
        // Only on a `[clipboard]` change: recreating the arboard handle
        // every reload would re-log the headless fallback, and neither
        // `use_os_clipboard` nor `osc_52` is mutable on the live
        // `Arc<dyn Clipboard>`.
        if cfg.clipboard != self.clipboard_cfg {
            self.clipboard = super::build_clipboard(&cfg.clipboard);
            self.clipboard_cfg = cfg.clipboard;
        }
        // After the renderer / title-prefix paths so a partial reload
        // mid-method does not leave the keymap on a stale `cfg` slot.
        self.keymap = Keymap::default_for_platform()
            .with_overrides(cfg.keymap.compile(cfg.source_dir.as_deref()));
        self.redraw.request();
        info!(font_changed, "config reloaded via Ctrl+Shift+R");
    }

    pub(crate) fn paste_from_clipboard(&mut self) {
        match self.clipboard.read() {
            Ok(bytes) => self.ferry_paste(&bytes),
            Err(err) => warn!(?err, "clipboard read"),
        }
    }

    /// `InputMsg::Paste`, not `KeyBytes`: the `?2004` framing belongs to
    /// the daemon, which reads the authoritative grid; a client-side wrap
    /// would frame against the shadow's mirror, which can lag the mode the
    /// program is in. The viewport snap matches [`Self::ferry_key_bytes`].
    pub(crate) fn ferry_paste(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // Checked before the confirmation gate so nothing is asked
        // about a paste that cannot be sent.
        if !self.admit_paste(bytes.len()) {
            return;
        }
        // REQ-804: a multi-line paste with bracketed-paste off is a
        // shell-injection vector. The shadow's mirror of `?2004` is the
        // gate: under bracketed paste the text arrives framed and inert.
        if felis_client_core::confirm::paste_needs_confirmation(
            self.shadow.bracketed_paste(),
            bytes,
        ) {
            self.arm_confirm(PendingConfirm::Paste(bytes.to_vec()));
            return;
        }
        self.send_paste_now(bytes);
    }

    /// Checks whether `len` bytes may be pasted, displaying visible notice.
    /// Oversized pastes are refused rather than truncated (REQ-105a) to avoid
    /// running truncated commands silently. Guards all `InputMsg::Paste`
    /// sources before [`Self::send_input`] drops frames with only a log line.
    fn admit_paste(&mut self, len: usize) -> bool {
        let Some(notice) = paste_refusal(len) else {
            return true;
        };
        warn!(
            len,
            cap = felis_protocol::messages::MAX_PASTE_BYTES,
            "paste refused: over the per-operation limit"
        );
        // On the bar as well as in the log: from the keyboard a refusal
        // nobody reports is a chord that appears to do nothing.
        self.set_notice(notice);
        false
    }

    /// The post-gate half of [`Self::ferry_paste`].
    fn send_paste_now(&self, bytes: &[u8]) {
        self.snap_to_live();
        self.send_input(&InputMsg::Paste(bytes.to_vec()));
    }

    /// A second question is dropped rather than queued: it would resolve
    /// against whichever prompt the user believes is on screen.
    pub(crate) fn arm_confirm(&mut self, pending: PendingConfirm) {
        if self.pending_confirm.is_some() {
            tracing::debug!("confirmation already pending; ignoring");
            return;
        }
        self.pending_confirm = Some(pending);
        self.redraw.request();
    }

    /// `false` (fall through) when no bar is open. While one is, every key
    /// is consumed except a bare modifier press, so Shift on the way to
    /// `Y` doesn't dismiss the prompt.
    pub(crate) fn handle_confirm_key(&mut self, key: &Key, text: Option<&str>) -> bool {
        if self.pending_confirm.is_none() {
            return false;
        }
        if matches!(
            key,
            Key::Named(
                NamedKey::Shift
                    | NamedKey::Control
                    | NamedKey::Alt
                    | NamedKey::AltGraph
                    | NamedKey::Super
                    | NamedKey::Meta
                    | NamedKey::CapsLock
            )
        ) {
            return true;
        }
        let Some(pending) = self.pending_confirm.take() else {
            return false;
        };
        self.redraw.request();
        match decide_confirm_key(text) {
            ConfirmDecision::Confirm => match pending {
                PendingConfirm::KillSession => self.kill_attached_session(),
                PendingConfirm::Paste(bytes) => self.send_paste_now(&bytes),
            },
            ConfirmDecision::Cancel => {
                tracing::debug!("confirmation canceled");
            }
        }
        true
    }

    /// One bottom bar for two transient uses. An armed question outranks a
    /// notice: the question is waiting on a key, the notice only reports
    /// what a key already did.
    pub(crate) fn build_confirm_overlay(&self) -> Option<felis_render_wgpu::ConfirmOverlay> {
        let label = match self.pending_confirm.as_ref() {
            Some(pending) => pending.label(),
            None => self.notice.clone()?,
        };
        Some(felis_render_wgpu::ConfirmOverlay { label })
    }

    pub(crate) fn set_notice(&mut self, text: String) {
        self.notice = Some(text);
        self.redraw.request();
    }

    /// Called for every keypress, so it must stay cheap and idempotent.
    pub(crate) fn clear_notice(&mut self) {
        if self.notice.take().is_some() {
            self.redraw.request();
        }
    }

    /// The window's own connection speaks the `Window` mode, which the
    /// daemon's mode gate refuses `Ops::Destroy` from, so this opens a
    /// one-shot `Ops` connection like `felis sessions kill` does and lets
    /// the eviction push tear the window down.
    fn kill_attached_session(&self) {
        let carrier = self.reconnector.carrier.clone();
        let id = self.current_session_id;
        self.runtime.spawn(async move {
            match connect_carrier(carrier, Offer::ops(), RemoteSpawn::Refuse).await {
                Ok(mut conn) => match conn.destroy_session(format!("{id:032x}")).await {
                    Ok(resolved) => {
                        tracing::info!(?resolved, "kill_session: destroy acknowledged");
                    }
                    Err(err) => warn!(error = %err, "kill_session: destroy failed"),
                },
                Err(err) => warn!(error = %err, "kill_session: ops dial failed"),
            }
        });
    }

    /// Middle-click and Shift+Insert. macOS / Windows fall through to the
    /// trait default (empty payload), matching native terminals.
    pub(crate) fn paste_from_primary(&mut self) {
        match self.clipboard.read_primary() {
            Ok(bytes) => self.ferry_paste(&bytes),
            Err(err) => warn!(?err, "PRIMARY read"),
        }
    }

    /// Pressing the chord during an Active search restarts compose without
    /// dragging old matches forward.
    pub(crate) fn enter_search_compose(&mut self) {
        self.search = SearchUi {
            mode: SearchUiMode::Composing,
            ..SearchUi::default()
        };
        self.redraw.request();
    }

    /// A reply stream still in the event queue is discarded by the mode
    /// gate in the frame handler.
    pub(crate) fn cancel_search(&mut self) {
        self.stop_search_stream();
        self.search = SearchUi::default();
        self.redraw.request();
    }

    /// Without this the daemon keeps walking a scrollback nobody is
    /// watching. Exactly one terminal still lands, and
    /// [`AppEvent::SearchEnded`] ignores it because the bar does not
    /// hold that id.
    fn stop_search_stream(&mut self) {
        let Some(stream_id) = self.search.stream.take() else {
            return;
        };
        self.driver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel_stream(stream_id);
        self.send_control(&ConnToDaemonMsg::Cancel { stream_id });
    }

    /// `true` when the search bar consumed the key.
    pub(crate) fn handle_search_compose_key(&mut self, key: &Key, text: Option<&str>) -> bool {
        match key {
            Key::Named(NamedKey::Escape) => {
                self.cancel_search();
                true
            }
            Key::Named(NamedKey::Enter) => {
                if self.search.query.is_empty() {
                    // The daemon would reject an empty needle anyway.
                    self.cancel_search();
                } else {
                    self.commit_search_query();
                }
                true
            }
            Key::Named(NamedKey::Backspace) => {
                self.search.query.pop();
                self.redraw.request();
                true
            }
            // winit surfaces the IME-resolved string via `text`, which
            // already handles uppercase / dead-key composition.
            _ => {
                if let Some(t) = text
                    && !t.is_empty()
                    && t.chars().all(|c| !c.is_control())
                {
                    self.search.query.push_str(t);
                    self.redraw.request();
                    return true;
                }
                false
            }
        }
    }

    /// n / N (vim convention: `n` = deeper into scrollback, `N` = back
    /// toward live) and Esc. `true` when consumed.
    pub(crate) fn handle_search_active_key(&mut self, key: &Key, text: Option<&str>) -> bool {
        if matches!(key, Key::Named(NamedKey::Escape)) {
            self.cancel_search();
            return true;
        }
        // `n` / `N` arrive as `Key::Character`; winit hands upper-case via
        // `text` when Shift is held.
        let Some(ch) = text.and_then(|s| s.chars().next()) else {
            return false;
        };
        if text.is_some_and(|s| s.chars().count() != 1) {
            return false;
        }
        match ch {
            'n' => {
                self.advance_search_hit(SearchDirection::Older);
                true
            }
            'N' => {
                self.advance_search_hit(SearchDirection::Newer);
                true
            }
            _ => false,
        }
    }

    /// Wraps at both ends, matching kitty / wezterm.
    pub(crate) fn advance_search_hit(&mut self, direction: SearchDirection) {
        let Some(next) =
            next_search_index(self.search.current, self.search.matches.len(), direction)
        else {
            return;
        };
        self.search.current = Some(next);
        let line_index = search_hit_anchor_line(&self.search.matches[next]);
        let rows = self.shadow.screen().rows();
        let max_scroll = self.shadow.viewport_max().saturating_sub(u32::from(rows));
        let target = viewport_for_hit(line_index, rows, max_scroll);
        if target == self.shadow.viewport() {
            // Already on-screen: no viewport round-trip to ride a redraw
            // on, so flip the `is_current` color locally.
            self.redraw.request();
        } else {
            self.send_viewport(target);
        }
    }

    pub(crate) fn build_search_overlay(&self) -> Option<SearchOverlay> {
        if self.search.mode == SearchUiMode::Off {
            return None;
        }
        let label = self.format_search_label();
        let visible_hits = self.collect_visible_hits();
        Some(SearchOverlay {
            label,
            visible_hits,
        })
    }

    pub(crate) fn format_search_label(&self) -> String {
        format_search_label(&self.search)
    }

    pub(crate) fn collect_visible_hits(&self) -> Vec<SearchHitSpan> {
        visible_hit_spans(
            &self.search.matches,
            self.search.current,
            self.shadow.viewport(),
            self.shadow.screen().rows(),
        )
    }

    pub(crate) fn commit_search_query(&mut self) {
        // A re-typed query supersedes the one still streaming, or the
        // daemon walks two scrollbacks for one visible search.
        self.stop_search_stream();
        let msg = SearchToDaemonMsg::Query {
            query: self.search.query.clone(),
            options: felis_protocol::messages::SearchOptions::default(),
        };
        // Checked before `open_stream` rather than at send: opening advances
        // the client's stream counter, so a dropped frame desynchronizes stream
        // IDs and causes subsequent operations (or an Escape `Cancel`) to drop
        // the connection on correlation faults.
        if let Err(err) = codec::WireCodec::validate(&msg) {
            refuse_search(&mut self.search, err.to_string());
            self.redraw.request();
            return;
        }
        let opened = self
            .driver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .open_stream();
        let stream_id = match opened {
            Ok(stream_id) => stream_id,
            Err(err) => {
                tracing::warn!(?err, "search: no stream id left on this connection");
                return;
            }
        };
        self.search.matches.clear();
        self.search.current = None;
        self.search.streaming = true;
        self.search.error = None;
        self.search.mode = SearchUiMode::Active;
        self.search.stream = Some(stream_id);
        self.send_control_correlated(&msg, Correlation::stream(stream_id));
        self.redraw.request();
    }

    /// `false` when no binding matched or a contextual gate (bare-End at
    /// the live view) declined; the caller falls through to the encoder.
    pub(crate) fn dispatch_chord(&mut self, key: &Key) -> bool {
        let Some(chord) = keymap_adapter::winit_to_chord(key, self.modifiers) else {
            return false;
        };
        let Some(action) = self.keymap.resolve(&chord).cloned() else {
            return false;
        };
        match action {
            Action::Paste {
                from: ClipboardScope::System,
            } => {
                self.paste_from_clipboard();
            }
            Action::Paste {
                from: ClipboardScope::Primary,
            } => {
                self.paste_from_primary();
            }
            Action::Copy {
                what: ClipboardScope::System,
            } => {
                self.copy_selection_to_clipboard();
            }
            Action::Copy {
                what: ClipboardScope::Primary,
            } => {
                self.copy_selection_to_primary();
            }
            Action::Reload => {
                self.reload_config();
            }
            Action::FontSize(FontSizeStep::Increase) => {
                self.adjust_font_size(1.0);
            }
            Action::FontSize(FontSizeStep::Decrease) => {
                self.adjust_font_size(-1.0);
            }
            Action::FontSize(FontSizeStep::Reset) => {
                self.reset_font_size();
            }
            Action::Ipc(IpcAction::OpenScrollbackSearch) => {
                self.enter_search_compose();
            }
            Action::Ipc(IpcAction::SwitchSession { to }) => {
                self.switch_session(to);
            }
            Action::Ipc(IpcAction::NewSession) => {
                self.spawn_session();
            }
            Action::Detach => {
                self.request_detach("detach chord");
            }
            Action::PipeRegion {
                source,
                target,
                ansi,
            } => {
                self.start_pipe(source, target, ansi);
            }
            Action::RunCommand { command } => {
                self.start_run(&command);
            }
            Action::Scroll(step) => {
                // Bare `End` at the live bottom falls through to the
                // encoder so pagers / vim still see `kend`.
                let bare_end_at_live = matches!(key, Key::Named(NamedKey::End))
                    && self.modifiers == ModifiersState::empty()
                    && self.shadow.viewport_at_bottom();
                if bare_end_at_live {
                    return false;
                }
                self.apply_scroll(step);
            }
            Action::ScrollToPrompt(direction) => {
                // docs/explanation/data-model/scrollback.md: the daemon
                // resolves the target against its prompt marks. Always
                // consumed, even when the daemon will no-op.
                self.send_input(&InputMsg::JumpPrompt { direction });
            }
            // docs/explanation/input.md "Action mapping": a `send_string` is
            // a typed sequence, not a paste, so this bypasses `ferry_paste`.
            // `compile` already rejected undecodable escapes; consume the
            // chord either way rather than leaking the keystroke.
            Action::SendString { text, escapes } => match escapes.decode(&text) {
                Ok(bytes) if !bytes.is_empty() => self.ferry_key_bytes(bytes),
                Ok(_) => {}
                Err(err) => warn!(error = %err, "send_string: undecodable escape"),
            },
            Action::ToggleFullscreen => {
                if let Some(surface) = self.surface.as_ref() {
                    let next = if surface.window.fullscreen().is_some() {
                        None
                    } else {
                        Some(winit::window::Fullscreen::Borderless(None))
                    };
                    surface.window.set_fullscreen(next);
                }
            }
            // docs/explanation/input.md "Confirmation bar".
            Action::Ipc(IpcAction::KillSession) => {
                self.arm_confirm(PendingConfirm::KillSession);
            }
        }
        true
    }

    /// docs/explanation/input.md "Which daemon runs the command": the
    /// daemon serializes the region (only it holds the grid); every sink
    /// runs here, on the machine whose keymap named it.
    /// The `Selection` source never asks: the daemon grid holds no
    /// selection (principle 3), so `ansi` does not apply to it.
    pub(crate) fn start_pipe(&mut self, source: PipeRegionSource, target: PipeTarget, ansi: bool) {
        if self.refuse_nested_visit("pipe") || self.is_busy() {
            tracing::debug!("pipe or switch already in progress; ignoring chord");
            return;
        }
        let viewport = self.shadow.viewport();
        let Some(wire_source) = source.wire_source() else {
            let Some(sel) = self.selection() else {
                tracing::debug!("pipe selection: no active selection; ignoring");
                return;
            };
            let text = sel.extract_text(self.shadow.screen());
            if text.is_empty() {
                tracing::debug!("pipe selection: empty selection; ignoring");
                return;
            }
            // A selection is an arbitrary sub-rectangle, not a window onto
            // the buffer, so "where the user was looking" has no meaning.
            self.finish_pipe(&text.into_bytes(), None, target, viewport);
            return;
        };
        // The daemon ends the connection on a `Region::Request` that
        // names no id.
        let issued = self
            .driver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .issue_request();
        let request = match issued {
            Ok(request) => request,
            Err(err) => {
                tracing::warn!(?err, "pipe: no request id left on this connection");
                return;
            }
        };
        self.pipe_state = PipeState::Awaiting { target, viewport };
        self.send_control_correlated(
            &RegionToDaemonMsg::Request {
                source: wire_source,
                ansi,
            },
            Correlation::request(request),
        );
    }

    /// Reached from `RegionToClientMsg::Reply`, and directly for the `Selection`
    /// source.
    pub(crate) fn finish_pipe(
        &mut self,
        data: &[u8],
        position: Option<RegionPosition>,
        target: PipeTarget,
        viewport: u32,
    ) {
        match target {
            PipeTarget::Command(argv) => {
                self.enter_transient(Some(data), &argv, position, viewport);
            }
            PipeTarget::File(path) => {
                self.pipe_state = PipeState::Idle;
                match write_region_file(data, path) {
                    // Info: with no user-named path this line is the only
                    // way the user learns where the bytes landed.
                    Ok(written) => info!(path = %written.display(), "pipe: wrote region to file"),
                    Err(err) => warn!(%err, "pipe: file sink failed"),
                }
                self.run_pending_push();
            }
            PipeTarget::Clipboard => {
                self.pipe_state = PipeState::Idle;
                // `write_user`: the user's own chord, ungated like
                // Ctrl+Shift+C, unlike a program's OSC 52 write
                // (`GridMsg::ClipboardSet`). The provenance is the request
                // this reply answers.
                if let Err(err) = self.clipboard.write_user(data) {
                    warn!(?err, "pipe-to-clipboard write");
                }
                self.run_pending_push();
            }
            PipeTarget::Paste => {
                self.pipe_state = PipeState::Idle;
                // The daemon brackets it per `?2004` exactly as a Ctrl+V.
                // Held to the same cap as one, too: its region budget is
                // twice `MAX_PASTE_BYTES`, so a large scrollback reaches
                // here already over the input limit.
                if self.admit_paste(data.len()) {
                    self.send_input(&InputMsg::Paste(data.to_vec()));
                }
                self.run_pending_push();
            }
        }
    }

    /// docs/explanation/input.md "Action mapping".
    pub(crate) fn start_run(&mut self, command: &[String]) {
        if self.refuse_nested_visit("run") || self.is_busy() {
            tracing::debug!("run or switch already in progress; ignoring chord");
            return;
        }
        if command.is_empty() {
            tracing::debug!("run: empty command; ignoring");
            return;
        }
        let viewport = self.shadow.viewport();
        self.enter_transient(None, command, None, viewport);
    }

    /// Spawn `argv` as a transient session on this client's local daemon.
    /// The dial adopts the local carrier even when the window is already
    /// remote, which is what lets a chord fired in a remote-attached
    /// window run here and still return there.
    fn enter_transient(
        &mut self,
        region: Option<&[u8]>,
        argv: &[String],
        position: Option<RegionPosition>,
        viewport: u32,
    ) {
        let origin = felis_client_core::pipe::Origin {
            session_id: self.current_session_id,
            host: match &self.reconnector.carrier {
                Carrier::Ssh { destination, .. } => Some(destination.clone()),
                Carrier::Local(_) => None,
            },
            osc7: self.shadow.cwd().map(str::to_owned),
        };
        let dims = GridDims {
            rows: self.shadow.screen().rows(),
            cols: self.shadow.screen().cols(),
            pixel_w: 0,
            pixel_h: 0,
        };
        let prepared =
            match felis_client_core::pipe::prepare_spawn(region, argv, dims, position, &origin) {
                Ok(prepared) => prepared,
                Err(err) => {
                    warn!(%err, "pipe: could not stage the region for the command");
                    self.pipe_state = PipeState::Idle;
                    self.run_pending_push();
                    return;
                }
            };
        // A window already on a local daemon keeps it: reaching past it to
        // the default socket would strand a `--socket` window's transients
        // on a daemon it never asked for.
        let socket = match self.reconnector.carrier.local_socket() {
            Some(socket) => Ok(socket),
            None => resolve_local_socket(None),
        };
        let socket = match socket {
            Ok(path) => path,
            Err(err) => {
                warn!(%err, "pipe: cannot resolve the local daemon socket");
                self.pipe_state = PipeState::Idle;
                self.run_pending_push();
                return;
            }
        };
        self.pipe_state = PipeState::Active {
            viewport,
            region: prepared.region,
            source: self.current_place(),
        };
        let reconnector = Reconnector {
            carrier: Carrier::Local(socket.clone().into()),
            offer: Offer::window(true),
        };
        self.switch_state = SwitchState::InFlight {
            session_gone: false,
            retry: None,
        };
        self.dial_in_background(
            dial_and_land(
                Carrier::Local(socket.into()),
                Offer::window(true),
                Landing::Create(prepared.args),
            ),
            Some(reconnector),
            TrailRecord::PushIntoTransient,
        );
    }

    /// Every way a transient's exit reaches the client must route through
    /// here: a path that skips it leaves `pipe_state` at `Active`, where
    /// every later handoff chord is silently dropped.
    pub(crate) fn park_pipe_return(&mut self, viewport: u32) {
        let PipeState::Active { region, source, .. } =
            std::mem::replace(&mut self.pipe_state, PipeState::Idle)
        else {
            return;
        };
        drop(region);
        self.pipe_state = PipeState::Returning {
            viewport,
            expected: source,
        };
    }

    pub(crate) fn spawn_session(&mut self) {
        if self.is_busy() {
            tracing::debug!("session switch already in progress; ignoring chord");
            return;
        }
        let carrier = self.reconnector.carrier.clone();
        let offer = self.reconnector.offer;
        // A local carrier resolves the same cwd fallback as the initial
        // window; an SSH-attached window must not ship this host's paths
        // across (see `launch_args`).
        let args = match launch_args(Vec::new(), &carrier) {
            Ok(args) => args,
            Err(err) => {
                // On the bar, not only in a log: a chord that fails silently
                // looks like a chord that did nothing.
                warn!("session spawn refused: {err}");
                self.set_notice(format!("session spawn: {err}"));
                return;
            }
        };
        info!("session spawn starting");
        self.switch_state = SwitchState::InFlight {
            session_gone: false,
            retry: None,
        };
        self.dial_in_background(
            dial_and_land(carrier, offer, Landing::Create(args)),
            None,
            TrailRecord::Push,
        );
    }

    /// Order matters: abort the active reader before the swap, queue `Detach`
    /// on the active writer, drop prior channels, and install the new.
    pub(crate) fn install_new_session(
        &mut self,
        reader: FrameReader<CarrierReader>,
        writer: FrameWriter<CarrierWriter>,
        driver: SharedDriver,
        attach: &SessionInfo,
        pull_enabled: bool,
    ) {
        // A confirmation parked against the previous session must not survive:
        // the question the user reads is not the one `y` answers.
        self.pending_confirm = None;
        self.notice = None;
        // The generation bump is what holds the line: `abort` is
        // best-effort (see `Pumps::abort_reader`).
        self.conn_gen += 1;
        if let Some(pump) = self.pump.as_ref() {
            pump.abort_reader();
        }
        // The writer task picks this up before the sender drop closes the
        // channel, so it reliably goes out.
        self.send_control(&SessionToDaemonMsg::Detach);
        self.outgoing.take();
        // Dropped through `retire`, not by the take alone: the sender is
        // gone now, but a writer parked on a stalled carrier would never
        // notice, and a detached task holds its carrier forever.
        if let Some(pump) = self.pump.take() {
            pump.retire();
        }
        // Image atlas ids and search line indices are session-scoped.
        // `self.shadow` is NOT reset: `RehydrateBegin` blanks its grid, and
        // retaining the existing grid until then gives a frozen previous view
        // rather than a blank flash.
        self.image_shadow = ImageShadow::new();
        self.search = SearchUi::default();
        // A fetch outstanding on the replaced connection can never answer.
        self.pending_switch.cancel();
        // There is no roster to update: the next chord's own fetch sees
        // the session this one just left go back into the pool.
        let anchor = RingKey::of(attach);
        let attached_dims = attach.dims;
        // Dropping the ladder here is what restores its ring rung.
        self.exit_ladder = None;
        self.current_sequence = anchor.sequence;
        self.current_session_id = anchor.id;
        // A retarget can cross carriers (local pulls, SSH eager-pushes).
        // The gate outlives the scheduler: the dead connection's last
        // cycle already reached the shadow, and `RehydrateBegin` on the
        // replacement is the first frame that redescribes the screen.
        let held = self.pull.cycle_open();
        self.pull = super::pull::PullScheduler::new(pull_enabled).holding_gate(held);
        // The rehydrate burst carries no dimensions of its own; without
        // this it would blank at the outgoing session's size and every row
        // would land short. Only the announcement moves here.
        self.shadow
            .announce_dims(attached_dims.rows, attached_dims.cols);
        // The reader starts pumping the rehydrate burst (already buffered
        // in the OS socket) as soon as `Pumps::spawn` runs.
        let (tx, rx) = crate::outgoing_channel();
        self.outgoing = Some(tx);
        // Ids are per-connection, so a stream the departed connection had
        // open means nothing here.
        self.driver = std::sync::Arc::clone(&driver);
        self.search = SearchUi::default();
        self.pump = Some(Pumps::spawn(
            reader,
            writer,
            rx,
            driver,
            self.proxy.clone(),
            self.conn_gen,
            self.reconnector.clone(),
        ));
        // Each fresh connection needs the theme for its `OSC 11 ; ?` replies.
        self.send_configure_theme();
        // Likewise for DECSET 2031 programs.
        self.report_color_scheme();
        self.switch_state = SwitchState::Idle;
        // The daemon started this session at whatever geometry it last
        // knew; the shell needs the right SIGWINCH before the user types.
        self.reflow_to_window(false);
        info!(
            id = format!("{:#x}", anchor.id),
            rows = attached_dims.rows,
            cols = attached_dims.cols,
            "session switched"
        );
    }

    /// Best-effort: a missing `outgoing` (mid-switch gap) drops the hint.
    pub(crate) fn send_configure_theme(&self) {
        if let Ok(frame) = crate::configure_theme_frame(&self.renderer_cfg) {
            self.enqueue(frame);
        }
    }

    /// For `DECSET 2031` / `DSR ? 996 n` programs (kitty / ghostty /
    /// contour convention). winit returns `None` when the platform can't
    /// report a preference; skip rather than guess.
    pub(crate) fn report_color_scheme(&self) {
        let Some(surface) = self.surface.as_ref() else {
            return;
        };
        let Some(theme) = surface.window.theme() else {
            return;
        };
        self.send_input(&InputMsg::ColorScheme {
            dark: theme == winit::window::Theme::Dark,
        });
    }

    /// The one path that coalesces: [`coalesce_key`] decides from the
    /// variant alone which queued frame a new one supersedes, and
    /// [`seals_key`] which queued slot this one closes behind it.
    pub(crate) fn send_input(&self, msg: &InputMsg) {
        // The encode happens here rather than in `FrameWriter::send`,
        // so an over-limit paste or raw payload costs the window its
        // carrier rather than one frame unless it is dropped here.
        match OutgoingFrame::input(msg) {
            Ok(frame) => self.enqueue(frame),
            Err(err) => warn!(%err, "dropping an over-limit outgoing frame"),
        }
    }

    /// The daemon clamps; the shadow's `viewport()` updates on the
    /// round-trip `GridMsg::ViewportState`.
    pub(crate) fn send_viewport(&self, lines_from_bottom: u32) {
        self.send_input(&InputMsg::Viewport { lines_from_bottom });
    }

    /// No-op at the bottom so redundant `Viewport` frames are not sent.
    pub(crate) fn snap_to_live(&self) {
        if self.shadow.viewport() > 0 {
            self.send_viewport(0);
        }
    }

    /// Typing after browsing scrollback drops you at the prompt (kitty /
    /// xterm `scrollKey`). The snap precedes the bytes on the same ordered
    /// stream, so the daemon composes the live view before the shell's
    /// response lands.
    pub(crate) fn ferry_key_bytes(&self, bytes: Vec<u8>) {
        self.snap_to_live();
        self.send_input(&InputMsg::KeyBytes(bytes));
    }

    /// The keystroke itself, for the daemon to encode against the
    /// keyboard modes it owns. Viewport snapping matches
    /// [`Self::ferry_key_bytes`] for the events
    /// [`crate::input::snaps_to_live`] admits.
    pub(crate) fn ferry_key(&self, event: felis_protocol::messages::KeyEvent) {
        // Composed text past the per-key cap is an IME commit wearing a
        // keystroke's clothes; admission would refuse it, so it takes
        // the path a commit already takes.
        if event
            .text
            .as_ref()
            .is_some_and(|text| text.len() > felis_protocol::limits::MAX_KEY_TEXT_BYTES)
        {
            if let Some(text) = event.text {
                self.ferry_key_bytes(text.into_bytes());
            }
            return;
        }
        if crate::input::snaps_to_live(&event) {
            self.snap_to_live();
        }
        self.send_input(&InputMsg::Key(event));
    }

    /// Reads `viewport()` / `viewport_max()` off the shadow rather than
    /// tracking client-side state: the daemon is authoritative and a stale
    /// shadow resyncs on the next `ViewportState`.
    pub(crate) fn apply_scroll(&self, step: felis_client_core::action::ScrollStep) {
        use felis_client_core::action::ScrollStep;
        let viewport = self.shadow.viewport();
        let rows = self.shadow.screen().rows();
        // The largest legal offset is the scrollback portion alone;
        // saturate in case an early-handshake shadow has viewport_max < rows.
        let max_scroll = self.shadow.viewport_max().saturating_sub(u32::from(rows));
        // Half-page jumps match Kitty's `scroll_half_page` default; floor
        // at 1 so a 1-row grid still scrolls.
        let half_page = u32::from(rows / 2).max(1);
        let new_viewport = match step {
            ScrollStep::LineUp => viewport.saturating_add(1).min(max_scroll),
            ScrollStep::LineDown => viewport.saturating_sub(1),
            ScrollStep::HalfPageUp => viewport.saturating_add(half_page).min(max_scroll),
            ScrollStep::HalfPageDown => viewport.saturating_sub(half_page),
            ScrollStep::Home => max_scroll,
            ScrollStep::End => 0,
        };
        if new_viewport != viewport {
            self.send_viewport(new_viewport);
        }
    }

    /// Positive = into history, clamped to `[0, max_scroll]`.
    pub(crate) fn scroll_by_rows(&self, delta_rows: i32) {
        if delta_rows == 0 {
            return;
        }
        let viewport = i64::from(self.shadow.viewport());
        let max_scroll = i64::from(
            self.shadow
                .viewport_max()
                .saturating_sub(u32::from(self.shadow.screen().rows())),
        );
        let new_viewport = (viewport + i64::from(delta_rows)).clamp(0, max_scroll) as u32;
        if new_viewport != self.shadow.viewport() {
            self.send_viewport(new_viewport);
        }
    }

    /// The kind comes from `WireCodec::KIND`, so shipping a message under
    /// the wrong kind is unrepresentable here.
    pub(crate) fn send_control<
        M: codec::WireCodec + felis_protocol::MinorGated + felis_protocol::messages::Directed + Clone,
    >(
        &self,
        msg: &M,
    ) {
        match OutgoingFrame::ordered(msg) {
            Ok(frame) => self.enqueue(frame),
            Err(err) => warn!(%err, "dropping an unsendable outgoing frame"),
        }
    }

    /// Drops frames when `outgoing` is missing mid-switch. If the queue is
    /// full, treats the carrier as lost and closes the connection rather
    /// than blocking the event loop (docs/explanation/architecture/session-lifecycle.md).
    fn enqueue(&self, frame: OutgoingFrame) {
        let Some(tx) = self.outgoing.as_ref() else {
            return;
        };
        if let Err(full) = tx.send(frame) {
            warn!(
                queued = full.queued,
                incoming = full.incoming,
                cap = full.cap,
                "outgoing queue full; treating the carrier as lost"
            );
            drop(self.proxy.send_event(AppEvent::DaemonClosed {
                conn_gen: self.conn_gen,
            }));
        }
    }

    pub(crate) fn send_control_correlated<
        M: codec::Correlated + felis_protocol::MinorGated + felis_protocol::messages::Directed + Clone,
    >(
        &self,
        msg: &M,
        correlation: Correlation,
    ) {
        match OutgoingFrame::correlated(msg, correlation) {
            Ok(frame) => self.enqueue(frame),
            Err(err) => warn!(%err, "dropping an unsendable outgoing frame"),
        }
    }

    /// Returns `None` only before the renderer is ready; clamping is
    /// [`cell_for_position`]'s.
    pub(crate) fn position_to_cell(&self, position: PhysicalPosition<f64>) -> Option<GridPos> {
        let renderer = &self.surface.as_ref()?.renderer;
        let metrics = renderer.cell_metrics();
        Some(cell_for_position(
            position,
            renderer.content_origin_px(),
            metrics.width,
            metrics.height,
            GridDims {
                rows: self.shadow.screen().rows(),
                cols: self.shadow.screen().cols(),
                pixel_w: 0,
                pixel_h: 0,
            },
        ))
    }

    /// `None` before the renderer is ready; the convention is
    /// [`pixel_for_position`]'s.
    pub(crate) fn position_to_pixel(&self, position: PhysicalPosition<f64>) -> Option<(u16, u16)> {
        let renderer = &self.surface.as_ref()?.renderer;
        Some(pixel_for_position(position, renderer.content_origin_px()))
    }

    /// The single place the client's 0-based [`GridPos`] becomes the
    /// wire's 1-based `(x=col, y=row)`: the X10/SGR mouse encodings are
    /// 1-based and the daemon-side encoder emits it unchanged. The pixel
    /// pair is for `?1016`.
    pub(crate) fn emit_mouse(
        &self,
        button: Option<MouseButton>,
        action: MouseAction,
        cell: GridPos,
    ) {
        // Only `?1016` reads the pixel pair, and a real mouse gesture
        // always follows a motion that fills it.
        let (px, py) = self
            .cursor_px
            .unwrap_or_else(|| (cell.col.saturating_add(1), cell.row.saturating_add(1)));
        let event = MouseEvent {
            button,
            action,
            mods: winit_keys::mods(self.modifiers).to_input_mods(),
            x: cell.col.saturating_add(1),
            y: cell.row.saturating_add(1),
            px,
            py,
        };
        self.send_input(&InputMsg::Mouse(event));
    }
}

/// Wraps at both ends, matching kitty / wezterm. `matches` is
/// newest-first, so a first `n` starts at the youngest hit and a first
/// `N` at the oldest.
pub(crate) const fn next_search_index(
    current: Option<usize>,
    len: usize,
    direction: SearchDirection,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (current, direction) {
        (None, SearchDirection::Older) => 0,
        (None, SearchDirection::Newer) => len - 1,
        (Some(i), SearchDirection::Older) => (i + 1) % len,
        (Some(i), SearchDirection::Newer) => (i + len - 1) % len,
    })
}

/// The first painted segment's row, not the logical line's top: a
/// stitched line's match may start rows below `line_index`, and centering
/// the top could leave the highlight off-screen.
pub(crate) fn search_hit_anchor_line(hit: &SearchHitRecord) -> i64 {
    hit.col_spans
        .first()
        .map_or(hit.line_index, |seg| seg.line_index)
}

/// Off-screen segments are dropped; n/N scrolls the viewport to bring one
/// into view on demand.
pub(crate) fn visible_hit_spans(
    matches: &[SearchHitRecord],
    current: Option<usize>,
    viewport: u32,
    rows: u16,
) -> Vec<SearchHitSpan> {
    let mut spans = Vec::new();
    for (idx, hit) in matches.iter().enumerate() {
        let is_current = current == Some(idx);
        for seg in &hit.col_spans {
            if seg.col_end <= seg.col_start {
                continue;
            }
            let Some(row) = composed_row_for_line(seg.line_index, viewport, rows) else {
                continue;
            };
            spans.push(SearchHitSpan {
                row,
                col_start: seg.col_start,
                col_end: seg.col_end,
                is_current,
            });
        }
    }
    spans
}

/// `origin_px` is the renderer's letterbox origin (same-user mirroring,
/// docs/explanation/architecture/session-lifecycle.md). A position outside
/// the block clamps to its edge cell, so a stray event mid-resize still
/// names a cell the grid has.
pub(crate) fn cell_for_position(
    position: PhysicalPosition<f64>,
    origin_px: (f32, f32),
    cell_w: u32,
    cell_h: u32,
    dims: GridDims,
) -> GridPos {
    let cw = cell_w.max(1);
    let ch = cell_h.max(1);
    let px = (position.x - f64::from(origin_px.0)).max(0.0) as u32;
    let py = (position.y - f64::from(origin_px.1)).max(0.0) as u32;
    let col0 = (px / cw).min(u32::from(dims.cols).saturating_sub(1));
    let row0 = (py / ch).min(u32::from(dims.rows).saturating_sub(1));
    GridPos {
        row: row0 as u16,
        col: col0 as u16,
    }
}

/// The 1-based, content-block-relative pixel the `?1016` SGR-pixel mouse
/// encoding reports, clamped into `u16` for the wire.
pub(crate) fn pixel_for_position(
    position: PhysicalPosition<f64>,
    origin_px: (f32, f32),
) -> (u16, u16) {
    let px = (position.x - f64::from(origin_px.0)).max(0.0);
    let py = (position.y - f64::from(origin_px.1)).max(0.0);
    // xterm reports pixels 1-based, mirroring the cell convention.
    let clamp = |v: f64| -> u16 { (v as u32).saturating_add(1).min(u32::from(u16::MAX)) as u16 };
    (clamp(px), clamp(py))
}

/// `Some(notice)` for a paste past [`MAX_PASTE_BYTES`], `None` for one
/// the window may send. Free-standing so the boundary is testable
/// without an `App`, which owns a surface and a GPU device.
///
/// [`MAX_PASTE_BYTES`]: felis_protocol::messages::MAX_PASTE_BYTES
pub(crate) fn paste_refusal(len: usize) -> Option<String> {
    let cap = felis_protocol::messages::MAX_PASTE_BYTES;
    (len > cap).then(|| format!("paste refused: {len} bytes, over the {cap}-byte limit"))
}

/// The bar's state for a query the client itself refuses: `reason` shows
/// where a daemon-side error would, and `stream` stays `None` because no
/// stream was opened for it.
pub(crate) fn refuse_search(search: &mut SearchUi, reason: String) {
    search.matches.clear();
    search.current = None;
    search.streaming = false;
    search.stream = None;
    search.error = Some(reason);
    search.mode = SearchUiMode::Active;
}

/// Single line, no newlines: the bar renderer paints chars left-to-right
/// until the right edge clips.
pub(crate) fn format_search_label(search: &SearchUi) -> String {
    match search.mode {
        SearchUiMode::Off => String::new(),
        SearchUiMode::Composing => format!("find: {}_", search.query),
        SearchUiMode::Active => {
            if let Some(err) = search.error.as_deref() {
                return format!("find: {}  error: {}", search.query, err);
            }
            let total = search.matches.len();
            if search.streaming {
                return format!("find: {}  ({}…)", search.query, total);
            }
            if total == 0 {
                return format!("find: {}  no matches", search.query);
            }
            match search.current {
                Some(i) => format!("find: {}  {}/{}", search.query, i + 1, total),
                None => format!("find: {}  {} matches", search.query, total),
            }
        }
    }
}

/// The window title carries the disconnected state because it is the
/// only chrome felis owns that survives a dead transport: no frame
/// arrives to paint an in-grid banner, and a banner drawn over the
/// shadow would overwrite the last thing the user saw.
pub(crate) fn window_title_for(
    prefix: Option<&str>,
    shell_title: &str,
    reconnecting: bool,
) -> String {
    let title = compose_window_title(prefix, shell_title);
    if reconnecting {
        return format!("{title} \u{2014} disconnected");
    }
    title
}

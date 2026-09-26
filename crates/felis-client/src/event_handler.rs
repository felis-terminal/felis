//! The winit `ApplicationHandler` implementation for `App`.

use felis_client_core::{
    pull::RedrawTurn,
    renderer_effect::{RendererEffect, renderer_effect},
};

use super::{
    ActiveEventLoop, App, AppEvent, ApplicationHandler, Arc, ClosedOutcome, ControlFlow, DragState,
    Duration, ElementState, GridMsg, ImageData, ImageId, ImageMsg, ImageSource, InputMsg, Instant,
    KeyEvent, ModifiersState, MouseAction, MouseButton, MouseScrollDelta, OutgoingSender,
    PipeState, PressRouting, Renderer, SearchDirection, SearchHitRecord, SearchToClientMsg,
    SearchUiMode, Selection, SelectionMode, SessionHex, Surface, SurfaceError, SwitchState,
    UserAttentionType, WheelEncoding, WindowEvent, WindowId, WinitMouseButton,
    activation_matches_preview, activation_target, apply_window_backdrop, compose_window_title,
    daemon_closed_outcome, error, forwarded_press_dismisses_selection,
    grid_change_invalidates_selection, grid_msg_moves_hover_target, ime_commit_bytes, info, input,
    map_button, mem, open_url, should_start_selection, warn, wheel_arrow_bytes, wheel_buttons,
    wheel_encoding_for, wheel_pixels_to_rows, wheel_starts_new_stream, wheel_y_ticks,
    wheel_zoom_delta_px, window_attributes,
};

fn apply_renderer_effect(renderer: &mut Renderer, effect: RendererEffect) {
    match effect {
        RendererEffect::ThemeOverride { channel, rgb } => {
            renderer.apply_theme_override(channel, rgb);
        }
        RendererEffect::PaletteOverride { index, rgb } => {
            renderer.apply_palette_override(index, rgb);
        }
        RendererEffect::ResetPalette => renderer.reset_palette_overrides(),
        RendererEffect::ResetSessionColors => renderer.reset_session_colors(),
        RendererEffect::ReverseVideo(on) => renderer.set_reverse_video(on),
    }
}

/// A newtype because the trait belongs to the renderer and the shadow to
/// client-core, which sits above the renderer in the dependency order.
struct ShadowImages<'a>(&'a felis_client_core::ImageShadow);

impl ImageSource for ShadowImages<'_> {
    fn image_data(&self, id: ImageId) -> Option<ImageData<'_>> {
        let img = self.0.image(id)?;
        Some(ImageData {
            width: img.width,
            height: img.height,
            format: img.format,
            pixels: img.pixels(),
        })
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.surface.is_some() {
            return;
        }
        let attrs = window_attributes(
            compose_window_title(self.title_prefix.as_deref(), "felis"),
            self.decorations,
            self.transparent,
        );
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(err) => {
                error!(?err, "create_window failed; exiting");
                event_loop.exit();
                return;
            }
        };
        // Insertion order does NOT decide who composites on top on macOS:
        // AppKit re-sorts the sibling order on every layout pass, so
        // `macos_window`'s zPosition fix is what keeps the terminal above
        // the backdrop.
        apply_window_backdrop(window.as_ref(), self.backdrop, self.transparent);
        let size = window.inner_size();
        // Before building the renderer, so `font_size_physical_px` already
        // reflects it: otherwise a Retina display rasterizes glyphs at half
        // size and the `pixel_w/pixel_h` shipped via TIOCGWINSZ advertises
        // sub-resolution cells that producers like yazi pre-scale to.
        self.scale_factor = window.scale_factor();
        let mut renderer_cfg = self.renderer_cfg.clone();
        renderer_cfg.font_size_physical_px = Some(self.font_size_physical_px());
        let renderer = match self.runtime.block_on(Renderer::new_with_config(
            window.clone(),
            size.width,
            size.height,
            renderer_cfg,
            self.warmup.take(),
        )) {
            Ok(r) => r,
            Err(err) => {
                error!(?err, "renderer init failed; exiting");
                event_loop.exit();
                return;
            }
        };
        // Only now does the wgpu surface's CAMetalLayer exist, so this is
        // the earliest the chrome repair can run.
        #[cfg(target_os = "macos")]
        if self.transparent {
            crate::macos_window::fix_translucent_layer_stack(window.as_ref());
        }
        info!(
            width = size.width,
            height = size.height,
            scale_factor = self.scale_factor,
            cell_w = renderer.cell_metrics().width,
            cell_h = renderer.cell_metrics().height,
            "window + surface ready"
        );
        // The RSS jump between `config-loaded` and here is the GPU / driver
        // footprint the heap profiler can't see.
        mem::log_rss("renderer-ready");
        // `FELIS_STARTUP_EXIT_MS`: exit after the first painted frame so a
        // non-interactive run captures the warmed-up RSS and writes the
        // `dhat-heap` dump.
        if let Some(ms) = std::env::var("FELIS_STARTUP_EXIT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            let proxy = self.proxy.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(ms));
                // RSS is process-wide, so the timer thread's sample is as
                // valid as the loop's, and reliable unlike the exit below.
                mem::log_rss("before-exit");
                // Graceful exit first so `dhat-heap` flushes its JSON. macOS
                // only pumps `EventLoopProxy` user events when the app is
                // frontmost, so force-exit as a backstop after a grace.
                drop(proxy.send_event(AppEvent::StartupExit));
                std::thread::sleep(Duration::from_secs(2));
                std::process::exit(0);
            });
        }
        // Per implementation.md "winit", this single switch turns on
        // zwp_text_input_v3, XIM, AppKit, and TSF composition.
        window.set_ime_allowed(true);
        self.surface = Some(Surface { window, renderer });
        // A fresh window starts on the platform default arrow, so the
        // remembered icon would suppress the first `set_cursor` that
        // matches it.
        self.cursor_icon = None;
        self.reflow(size);
        // Otherwise the first composition sits at a stale platform default.
        self.update_ime_cursor_area();
        // A DECSET 2031 program needs the scheme before its first DSR 996.
        self.report_color_scheme();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => self.request_detach("window close requested"),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // Re-rasterize at the new physical-px size so glyphs stay
                // crisp and the next `pixel_w/pixel_h` matches the display.
                self.scale_factor = scale_factor;
                let physical = self.font_size_physical_px();
                if let Some(surface) = self.surface.as_mut() {
                    surface.renderer.reload_font_size(physical);
                }
                self.reflow_to_window(true);
                self.redraw.request();
                info!(scale_factor, "DPR changed; font rebuilt");
            }
            WindowEvent::Resized(size) => {
                if let Some(surface) = self.surface.as_mut() {
                    surface.renderer.resize(size.width, size.height);
                }
                // wgpu reconfigures the CAMetalLayer on resize and AppKit
                // rebuilds the root backing layer on layout, both dropping
                // the corner radius. See `macos_window`.
                #[cfg(target_os = "macos")]
                if self.transparent
                    && let Some(surface) = self.surface.as_ref()
                {
                    crate::macos_window::fix_translucent_layer_stack(surface.window.as_ref());
                }
                self.after_metric_change(size);
            }
            WindowEvent::ModifiersChanged(state) => {
                self.modifiers = state.state();
                // Re-evaluate Ctrl+hover now, not on the next CursorMoved.
                self.update_mouse_cursor_icon();
            }
            WindowEvent::ThemeChanged(theme) => {
                // felis's own colors are config/OSC-driven and unaffected.
                self.send_input(&InputMsg::ColorScheme {
                    dark: theme == winit::window::Theme::Dark,
                });
            }
            WindowEvent::Focused(focused) => {
                self.send_input(&InputMsg::FocusChange { focused });
                self.window_focused = focused;
                if let Some(surface) = self.surface.as_mut() {
                    surface.renderer.set_window_focused(focused);
                }
                // Solid-on so a re-focused window shows the caret at once.
                if focused {
                    self.restart_blink();
                }
                // Release events while another app has focus never reach
                // us, leaving modifiers stuck.
                if !focused {
                    self.modifiers = ModifiersState::empty();
                    self.update_mouse_cursor_icon();
                }
                self.redraw.request();
            }
            WindowEvent::Occluded(false) => self.present_retry.wake(Instant::now()),
            WindowEvent::CursorLeft { .. } => {
                // A re-entry's first CursorMoved must count as "new cell".
                self.cursor_cell = None;
                self.cursor_px = None;
                self.pointer_px = None;
                self.update_post_mouse_state();
                self.update_mouse_cursor_icon();
            }
            WindowEvent::CursorMoved { position, .. } => {
                // Before the cell test returns early: sub-cell motion still
                // moves a pointer-reactive shader.
                self.pointer_px = Some(position);
                self.update_post_mouse_state();
                let Some(cell) = self.position_to_cell(position) else {
                    return;
                };
                // Even within the same cell, so a button / wheel event
                // reports fresh sub-cell pixels under `?1016`.
                self.cursor_px = self.position_to_pixel(position);
                if self.cursor_cell == Some(cell) {
                    return;
                }
                self.cursor_cell = Some(cell);
                self.update_mouse_cursor_icon();
                // A selection drag stops before the mouse-to-PTY path so the
                // program doesn't also see it.
                let drag_button_held = self.held_buttons.is_held(MouseButton::Left)
                    || self.held_buttons.is_held(MouseButton::Right);
                if drag_button_held && let DragState::Selected(sel) = &mut self.drag {
                    sel.extend(cell);
                    self.update_renderer_selection();
                    return;
                }
                // CursorMoved only reaches here once the cursor crossed into
                // a new cell, so this is a genuine drag, not the click.
                if self.held_buttons.is_held(MouseButton::Left)
                    && let DragState::Pending(anchor, mode) = self.drag
                {
                    let mut sel = match mode {
                        SelectionMode::Linear => Selection::new(anchor),
                        SelectionMode::Rectangle => Selection::new_rectangle(anchor),
                    };
                    sel.extend(cell);
                    self.drag = DragState::Selected(sel);
                    self.update_renderer_selection();
                    return;
                }
                let action = if self.held_buttons.is_empty() {
                    MouseAction::Motion
                } else {
                    MouseAction::Drag
                };
                let button = self.held_buttons.last();
                self.emit_mouse(button, action, cell);
            }
            WindowEvent::MouseInput {
                state,
                button: winit_btn,
                ..
            } => self.on_mouse_input(state, winit_btn),
            WindowEvent::MouseWheel { delta, .. } => self.on_wheel(delta),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state,
                        text,
                        location,
                        repeat,
                        ..
                    },
                ..
            } => {
                let is_press = matches!(state, ElementState::Pressed);
                // A notice reports the previous keypress, so it dies with
                // this one: cleared before dispatch, since the handlers
                // below may raise a new one for the key being pressed.
                if is_press {
                    self.clear_notice();
                }
                // Keymap dispatch (docs/explanation/input.md): a hit consumes
                // the keypress, a miss falls through to the input encoder.
                // An open confirmation bar owns the keyboard outright,
                // chords included, so re-pressing `kill_session` or a paste
                // chord can't stack a second question under the first.
                if is_press && self.handle_confirm_key(&logical_key, text.as_deref()) {
                    return;
                }
                if is_press && self.dispatch_chord(&logical_key) {
                    return;
                }
                // Before the input encoder so the keys hit the search bar,
                // not the program (REQ-607).
                if is_press
                    && self.search.mode == SearchUiMode::Composing
                    && self.handle_search_compose_key(&logical_key, text.as_deref())
                {
                    return;
                }
                // Esc dismisses, `n` / `N` navigate; before the encoder so
                // they don't leak to the program.
                if is_press
                    && self.search.mode == SearchUiMode::Active
                    && self.handle_search_active_key(&logical_key, text.as_deref())
                {
                    return;
                }
                self.ferry_key(input::key_event(
                    &logical_key,
                    text.as_deref(),
                    self.modifiers,
                    input::event_kind(state, repeat),
                    location,
                ));
            }
            WindowEvent::Ime(ime) => {
                // The commit string is finalized UTF-8; the encoder's
                // modifier / disambiguation logic doesn't apply, so the bytes
                // ferry raw (like a paste). Pre-edit text routes to the
                // renderer overlay.
                self.apply_ime_event(&ime);
                if let Some(bytes) = ime_commit_bytes(&ime) {
                    self.ferry_key_bytes(bytes);
                }
            }
            // A cycle in flight is a screen state the daemon has not
            // finished describing, so no redraw source paints it: a
            // blink or an animation frame landing between a `Scrolled`
            // and its rows would put the shifted grid on screen without
            // the content that follows it.
            WindowEvent::RedrawRequested
                if may_paint_grid(
                    self.surface.is_some(),
                    self.shadow.is_rehydrating(),
                    self.pull.cycle_open(),
                ) =>
            {
                self.on_redraw(event_loop);
            }
            WindowEvent::DroppedFile(path) => {
                // Not OSC 72 (docs/explanation/input.md "Drag-and-drop"); no shell-aware
                // quoting (principle 4). Routed through the paste path so bracketed-paste
                // guards apply. Trailing space separates successive file drops.
                let mut bytes = dropped_path_bytes(&path);
                bytes.push(b' ');
                tracing::debug!(target: "felis::input", ?path, "file dropped");
                self.ferry_paste(&bytes);
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::GridFrame { msg, conn_gen } => {
                if conn_gen != self.conn_gen {
                    // Applying it would paint the previous session's content
                    // into the new session's shadow.
                    tracing::debug!(conn_gen, live = self.conn_gen, "stale grid frame; dropping");
                    return;
                }
                // `grid_change_invalidates_selection` needs the alt-screen
                // flag on both sides of the apply.
                let alt_screen_before = self.shadow.alt_screen();
                if let Err(err) = self.shadow.apply(&msg) {
                    // A value the frame layer cannot catch: `packed_cells`
                    // is opaque `bytes` to the envelope, and a coordinate
                    // is legal in its own field. Applying it would paint a
                    // screen the daemon never composed.
                    warn!(?err, "grid frame refused; closing the connection");
                    self.close_daemon_connection(event_loop, conn_gen);
                    return;
                }
                if self.shadow.take_title_dirty().is_some() {
                    self.refresh_window_title();
                }
                // A frame that moved cells or the link table can change
                // what is under a stationary pointer; the preview and
                // Ctrl+Click must agree on the same target, so this can't
                // wait for the next CursorMoved.
                if grid_msg_moves_hover_target(&msg) {
                    self.update_mouse_cursor_icon();
                }
                if let Some(write) = self.shadow.take_pending_clipboard_set() {
                    // Routed by the target bitmap the producer named (`c` /
                    // `p`); both program paths are hostile-gated in the
                    // backend. The program already issued the OSC, so
                    // retrying an unavailable backend is up to it.
                    if write
                        .selection
                        .contains(felis_grid::ClipboardSelection::CLIPBOARD)
                        && let Err(err) = self.clipboard.write(&write.data)
                    {
                        warn!(?err, selection = ?write.selection, "clipboard write");
                    }
                    if write
                        .selection
                        .contains(felis_grid::ClipboardSelection::PRIMARY)
                        && let Err(err) = self.clipboard.write_primary_program(&write.data)
                    {
                        warn!(?err, selection = ?write.selection, "primary clipboard write");
                    }
                }
                if let Some(effect) = renderer_effect(&msg)
                    && let Some(surface) = self.surface.as_mut()
                {
                    apply_renderer_effect(&mut surface.renderer, effect);
                }
                match &msg {
                    GridMsg::CursorState { .. } => {
                        self.update_ime_cursor_area();
                        // Blink restarts on activity (kitty / xterm).
                        self.restart_blink();
                    }
                    // A notification (docs/reference/protocols/notifications.md)
                    // flashes the same way BEL does: felis never draws a
                    // popup, and the content went to observer connections.
                    GridMsg::Attention { .. } => {
                        if let Some(surface) = self.surface.as_ref() {
                            surface
                                .window
                                .request_user_attention(Some(UserAttentionType::Informational));
                        }
                    }
                    _ => {}
                }
                // See `grid_change_invalidates_selection`.
                if matches!(self.drag, DragState::Selected(_))
                    && grid_change_invalidates_selection(
                        &msg,
                        alt_screen_before,
                        self.shadow.alt_screen(),
                    )
                {
                    self.drag = DragState::Idle;
                    self.update_renderer_selection();
                }
                // A compose-cycle frame answers the outstanding pull (facet
                // pushes ship outside the gate, `pull::answers_pull`).
                self.pull.on_frame(&msg);
                self.redraw.request();
            }
            AppEvent::SearchFrame { msg, conn_gen } => {
                if conn_gen != self.conn_gen {
                    tracing::debug!(
                        conn_gen,
                        live = self.conn_gen,
                        "stale search frame; dropping"
                    );
                    return;
                }
                // Mode-gated so a stale frame after Esc doesn't churn the bar.
                if self.search.mode != SearchUiMode::Active {
                    return;
                }
                let SearchToClientMsg::Match {
                    line_index,
                    col_spans,
                    ..
                } = msg;
                self.search.matches.push(SearchHitRecord {
                    line_index,
                    col_spans,
                });
                self.redraw.request();
            }
            AppEvent::SearchEnded {
                stream_id,
                outcome,
                conn_gen,
            } => {
                if conn_gen != self.conn_gen {
                    tracing::debug!(conn_gen, live = self.conn_gen, "stale search terminal");
                    return;
                }
                // A terminal for a stream this bar has moved on from must not
                // revive the bar or overwrite the newer search's state.
                if self.search.stream != Some(stream_id) {
                    tracing::debug!(%stream_id, "terminal for a superseded search stream");
                    return;
                }
                self.search.stream = None;
                self.search.streaming = false;
                match outcome {
                    Err(err) => {
                        warn!(error = %err, "search error from daemon");
                        self.search.error = Some(err);
                    }
                    Ok(_count) => {
                        if !self.search.matches.is_empty() && self.search.current.is_none() {
                            // Auto-jump to the youngest hit, as kitty / less
                            // do on `/foo<Enter>`.
                            self.advance_search_hit(SearchDirection::Older);
                        }
                    }
                }
                self.redraw.request();
            }
            AppEvent::ImageFrame { msg, conn_gen } => {
                if conn_gen != self.conn_gen {
                    // Image ids are session-scoped: a late frame would
                    // allocate against ids the new session reuses.
                    tracing::debug!(
                        conn_gen,
                        live = self.conn_gen,
                        "stale image frame; dropping"
                    );
                    return;
                }
                // Header / Chunk only build up the pixel buffer; redrawing
                // per chunk would turn one multi-MiB transmission into N
                // full frame encodes. ShowFrame is the animation tick.
                let needs_redraw = !matches!(msg, ImageMsg::Header { .. } | ImageMsg::Chunk { .. });
                // Producer-reported geometry, so a real-device session can
                // verify yazi/mdcat pre-scale as expected.
                match &msg {
                    ImageMsg::Header {
                        id,
                        target: felis_protocol::messages::ImageTarget::New { width, height, .. },
                    } => {
                        info!(image_id = id.0, width, height, "image header");
                    }
                    ImageMsg::Placement {
                        image_id,
                        placement_id,
                        cols,
                        rows,
                        ..
                    } => {
                        info!(
                            image_id = image_id.0,
                            placement_id = ?placement_id,
                            cols,
                            rows,
                            "image placement"
                        );
                    }
                    _ => {}
                }
                if let Err(err) = self.image_shadow.apply(&msg) {
                    // A header claiming memory the protocol does not
                    // allow: the same answer a row that did not decode
                    // gets (A-7), since a peer that orders one
                    // allocation it is not entitled to has already
                    // stopped being the authority the mirror trusts.
                    warn!(?err, "image claim refused; closing the connection");
                    self.close_daemon_connection(event_loop, conn_gen);
                    return;
                }
                if needs_redraw {
                    self.redraw.request();
                }
            }
            AppEvent::DaemonClosed { conn_gen } => self.on_daemon_closed(event_loop, conn_gen),
            AppEvent::Detached => {
                info!("detached from session; exiting (session survives)");
                event_loop.exit();
            }
            AppEvent::StartupExit => {
                // Unwind the loop cleanly so a `dhat-heap` build flushes its
                // JSON; only reached when macOS pumps the user event, else
                // the timer thread's force-exit backstop fires.
                info!("FELIS_STARTUP_EXIT_MS elapsed; exiting after startup measurement");
                event_loop.exit();
            }
            AppEvent::ConfigReloaded => {
                self.reload_config();
            }
            AppEvent::SessionSwitchReady {
                reader,
                writer,
                driver,
                attach,
                pull_enabled,
                record,
                retargeted,
            } => {
                if crate::exit_ladder::pushes_trail(
                    record,
                    self.on_transient,
                    self.switch_state.session_gone(),
                ) {
                    self.trail.push(self.current_place());
                }
                if let Some(new_reconnector) = retargeted {
                    self.reconnector = new_reconnector;
                }
                self.on_transient = crate::exit_ladder::transient_after(record, self.on_transient);
                self.install_new_session(reader, writer, driver, &attach, pull_enabled);
                if std::mem::take(&mut self.reconnecting) {
                    info!("reconnected to the session this window was on");
                    self.refresh_window_title();
                }
                self.run_pending_push();
                // After the push: a continuation it started is what says
                // the unwind has not settled, and only then is the
                // handoff not this landing's to drop.
                self.settle_handoff(crate::exit_ladder::carries_handoff(record));
            }
            AppEvent::ReconnectFailed { reason, detail } => {
                self.switch_state = SwitchState::Idle;
                self.reconnecting = false;
                let Some(reason) = reason else {
                    // The carrier dropped before `SessionExited` could
                    // arrive and the re-dial learned the verdict instead;
                    // the window owes the user the same unwind either way.
                    warn!(%detail, "the reconnect found the session gone; taking the exit ladder");
                    self.trail.prune(&self.current_place());
                    if self.begin_exit_ladder("the reconnect found the session gone") {
                        event_loop.exit();
                    }
                    return;
                };
                error!(%detail, remedy = reason.remedy(), "{}", reason.headline());
                // Read after the loop unwinds; the window closes either
                // way, so the status is all that is left to say it with.
                self.terminal_exit = Some(reason);
                event_loop.exit();
            }
            AppEvent::SessionSwitchFailed {
                reason,
                target_vanished,
            } => {
                let (session_gone, retry) = match self.switch_state {
                    SwitchState::InFlight {
                        session_gone,
                        retry,
                    } => (session_gone, retry),
                    SwitchState::Idle => (false, None),
                };
                self.switch_state = SwitchState::Idle;
                if target_vanished && let Some(direction) = retry {
                    // The roster the pick came from is provably stale, so
                    // B-11 spends one refetch, and exactly one: a second
                    // failure takes the branches below.
                    info!(reason, "switch target went away; re-picking once");
                    self.retry_switch(direction);
                } else if session_gone {
                    // Also where a landing that carried the exit starts
                    // the ladder: its deadline runs from here, not from
                    // an exit the landing may have outlived.
                    warn!(reason, "the session is gone and the landing failed");
                    if self.begin_exit_ladder("the landing failed") {
                        event_loop.exit();
                    }
                } else {
                    match std::mem::replace(&mut self.pipe_state, PipeState::Idle) {
                        PipeState::Active { region, .. } => {
                            warn!(reason, "pipe switch-in failed; staying on current session");
                            drop(region);
                        }
                        PipeState::Awaiting { .. } => {
                            warn!(reason, "pipe switch-in failed; staying on current session");
                        }
                        parked @ PipeState::Returning { .. } => {
                            warn!(reason, "session switch failed; staying on current session");
                            self.pipe_state = parked;
                        }
                        PipeState::Idle => {
                            warn!(reason, "session switch failed; staying on current session");
                        }
                    }
                }
                // A close or a queue overflow under the landing was
                // absorbed as `AwaitSwitch`. Staying on that connection
                // would put the window back on a carrier it cannot
                // speak over, so the outcome is decided here instead,
                // with the switch out of the way.
                if self.outgoing.as_ref().is_some_and(OutgoingSender::is_lost) {
                    warn!("the connection behind the failed switch is gone; closing it");
                    self.on_daemon_closed(event_loop, self.conn_gen);
                }
                self.run_pending_push();
                // A failed continuation is still a continuation ending:
                // whatever it left parked is settled here unless the
                // push it just ran started another.
                self.settle_handoff(false);
            }
            AppEvent::PipeRegionReady { data, position } => {
                // The sink is not echoed in the reply.
                let Some((target, viewport)) = self.pipe_state.take_awaiting() else {
                    tracing::debug!("unsolicited pipe region data; ignoring");
                    return;
                };
                if data.is_empty() {
                    // An unresolvable region (no OSC 133 range yet) and an
                    // empty one answer alike. The handoff is over either
                    // way, so a parked push runs now.
                    tracing::debug!("pipe: empty region; ignoring");
                    self.run_pending_push();
                    return;
                }
                self.finish_pipe(&data, position, target, viewport);
            }
            AppEvent::RosterListed {
                request,
                sessions,
                conn_gen,
            } => {
                if conn_gen != self.conn_gen {
                    // The swap already dropped the fetch that asked for it.
                    tracing::debug!(conn_gen, live = self.conn_gen, "stale session roster");
                    return;
                }
                self.apply_roster(request, &sessions);
            }
            AppEvent::RosterFetchTimedOut { request, conn_gen } => {
                // The timer always fires, including for fetches that
                // resolved long ago.
                if conn_gen != self.conn_gen || !self.pending_switch.awaiting(request) {
                    return;
                }
                warn!("the daemon did not answer the session roster query in time");
                self.abandon_fetch();
            }
            AppEvent::RequestRefused {
                request,
                detail,
                conn_gen,
            } => {
                if conn_gen != self.conn_gen {
                    tracing::debug!(conn_gen, live = self.conn_gen, "stale request refusal");
                    return;
                }
                if self.pending_switch.awaiting(request) {
                    // The pending-switch slot gates every later chord, so a
                    // refusal nobody cleared would leave the window unable
                    // to switch for the rest of the connection.
                    warn!(%detail, "daemon refused the session roster query");
                    self.abandon_fetch();
                    return;
                }
                warn!(%detail, "daemon refused the pipe region request");
                // `is_busy` gates the chord, so a sink nobody released would
                // make the window unable to pipe again.
                drop(self.pipe_state.take_awaiting());
                self.run_pending_push();
            }
            AppEvent::ReattachRequested { reconnector, id } => {
                let pending = crate::PendingReattach { reconnector, id };
                // The daemon declines to push a same-session switch, so this
                // is a push whose target the window reached while it was in
                // flight; re-attaching would rehydrate onto the same grid.
                if pending.place() == self.current_place() {
                    info!("reattach push targets the current session; ignoring");
                } else if self.is_busy() {
                    // The daemon already counted this push as delivered (the
                    // CLI exited 0); park it for when the transition lands.
                    info!(id = %SessionHex(id), "client busy; deferring reattach push");
                    self.pending_reattach = Some(pending);
                    self.pending_retarget = None;
                } else {
                    self.reattach_now(pending);
                }
            }
            AppEvent::RetargetRequested { target } => {
                // Same deferral contract as a reattach push. No same-id
                // short-circuit: the target lives on another daemon, so
                // "same id" is meaningless across the namespace boundary.
                if self.is_busy() {
                    info!("client busy; deferring retarget push");
                    self.pending_retarget = Some(target);
                    self.pending_reattach = None;
                } else {
                    self.retarget(target);
                }
            }
            AppEvent::SessionExited { conn_gen, place } => {
                self.trail.prune(&place);
                // A deliberate reattach the user made before this event
                // was handled stays on the corpse it named, as the plain
                // attach promises; only the live connection's own exit
                // moves the window.
                if conn_gen != self.conn_gen {
                    info!(
                        id = %SessionHex(place.session_id),
                        "SessionExited from a superseded connection; pruned the trail only"
                    );
                } else if self.switch_state.carry_exit() {
                    // Ahead of the transient branch: a pipe/run switch-in
                    // marks `Active` before its create lands, so starting
                    // the ladder here would run a second dial beside a
                    // create still in flight and unlink its region under it.
                    info!("shell exited during a switch; the in-flight landing carries the exit");
                } else if let PipeState::Active { viewport, .. } = self.pipe_state {
                    info!("run/pipe transient exited; returning to originating session");
                    self.park_pipe_return(viewport);
                    if self.begin_exit_ladder("the pipe/run transient exited") {
                        event_loop.exit();
                    }
                } else {
                    // A chord's fetch on the wire rides the connection this
                    // exit is tearing down; the ladder's own dial picks.
                    if self.pending_switch.is_fetching() {
                        info!("shell exited under a pending switch chord; the exit supersedes it");
                    }
                    self.pending_switch.cancel();
                    // The pump applied every queued row before handing this
                    // event over, so the shadow already holds the shell's
                    // last screen; ask for the frame in case one is still
                    // presented before the window goes.
                    self.redraw.request();
                    if self.begin_exit_ladder("shell exited") {
                        event_loop.exit();
                    }
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Before the redraw flush so a phase toggle's repaint rides this
        // iteration. A hidden cursor arms no timer, so the window stays
        // idle (ControlFlow::Wait).
        let blink_wants = {
            let grid = self.shadow.screen();
            let cur = grid.cursor();
            // A blinking caret on a disconnected window reads as a live
            // shell waiting for input.
            let shown = cur.visible
                && !self.reconnecting
                && self.window_focused
                && self.shadow.viewport() == 0
                && cur.row < grid.rows()
                && cur.col < grid.cols();
            shown && grid.cursor_blink()
        };
        let now = Instant::now();
        if self.cursor_blink.tick(now, blink_wants) {
            if let Some(surface) = self.surface.as_mut() {
                surface
                    .renderer
                    .set_cursor_blink_visible(self.cursor_blink.visible());
            }
            self.redraw.request();
        }
        // With no shader the clock would arm wake-ups for an animation no
        // pass draws.
        let trail_deadline = self.tick_cursor_trail(now);
        // Nothing arms this clock, so the predicate is the whole guard.
        let shader_animating = self.window_focused
            && self
                .surface
                .as_ref()
                .is_some_and(|surface| surface.renderer.has_post_shader());
        if self.shader_clock.tick(now, shader_animating) {
            self.redraw.request();
        }
        let shader_deadline = self.shader_clock.next_deadline(shader_animating);
        let turn = self.present_retry.turn(now);
        if turn == RedrawTurn::Retry {
            self.redraw.request();
        }
        // winit also coalesces per vsync, but one site keeps the request vs
        // flush counters observable and stops a new event source
        // sidestepping the coalescer.
        if turn != RedrawTurn::Hold
            && self.redraw.flush()
            && let Some(surface) = self.surface.as_ref()
        {
            surface.window.request_redraw();
            // `--trace-perf` users see how many events one paint collapsed.
            let stats = self.redraw.stats();
            tracing::trace!(
                target: "felis::redraw",
                requested = stats.requested,
                flushed = stats.flushed,
                "redraw flush",
            );
        }
        // The next-frame pull is NOT re-armed here: `about_to_wait` runs
        // once per inbound frame during a burst, far faster than vsync, so
        // pacing would become a tight request→reply loop that floods the
        // daemon with pulls (starving the PTY drain). It re-arms in
        // `RedrawRequested`, once per painted frame.

        // A steady or hidden cursor leaves `ControlFlow::Wait` untouched so
        // an idle window burns no CPU; the loop wakes at whichever clock is
        // due first.
        // While stalled the other clocks' redraws are held, so waking for
        // them would only spin the loop.
        let deadline = if self.present_retry.stalled() {
            self.present_retry.next_deadline()
        } else {
            [
                self.cursor_blink.next_deadline(blink_wants),
                trail_deadline,
                shader_deadline,
            ]
            .into_iter()
            .flatten()
            .min()
        };
        match deadline {
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.outgoing.take();
        self.surface.take();
    }
}

impl App {
    /// Whether the daemon closed it or this window did.
    fn on_daemon_closed(&mut self, event_loop: &ActiveEventLoop, conn_gen: u64) {
        let outcome = daemon_closed_outcome(
            conn_gen,
            self.conn_gen,
            self.switch_state.in_flight(),
            &self.pipe_state,
        );
        match outcome {
            ClosedOutcome::Ignore => {
                tracing::debug!(
                    conn_gen,
                    live = self.conn_gen,
                    "superseded connection closed; ignoring"
                );
            }
            ClosedOutcome::AwaitSwitch => {
                info!("daemon connection closed during a switch; awaiting the switch outcome");
                if let Some(outgoing) = self.outgoing.as_ref() {
                    outgoing.mark_lost();
                }
            }
            ClosedOutcome::PipeReturn { viewport } => {
                // The same ladder the typed exit takes, so no return
                // path sits outside the ladder's bound.
                info!("pipe command exited; returning to originating session");
                self.park_pipe_return(viewport);
                if self.begin_exit_ladder("the pipe/run transient's channel closed") {
                    event_loop.exit();
                }
            }
            ClosedOutcome::Reconnect => self.begin_reconnect(),
        }
    }

    /// `abort` is best-effort, so the generation bump inside
    /// `install_new_session` remains what holds the line on reconnect.
    fn close_daemon_connection(&mut self, event_loop: &ActiveEventLoop, conn_gen: u64) {
        if let Some(pump) = self.pump.as_ref() {
            pump.abort_reader();
        }
        self.on_daemon_closed(event_loop, conn_gen);
    }

    fn on_mouse_input(&mut self, state: ElementState, winit_btn: WinitMouseButton) {
        let Some(button) = map_button(winit_btn) else {
            return;
        };
        if state == ElementState::Pressed && button == MouseButton::Left {
            self.last_press_px = self.pointer_px;
        }
        self.update_post_mouse_state();
        let Some(cell) = self.cursor_cell else { return };
        // Ctrl+Left on an armed OSC 8 cell opens the URL without program forwarding.
        // Recomputed here rather than read from `hover_target` so the press acts on
        // the cell it landed in rather than the last hovered cell.
        if state == ElementState::Pressed
            && button == MouseButton::Left
            && let Some(target) = activation_target(
                &self.shadow,
                Some(cell),
                self.modifiers,
                self.link_preview_row_free(),
            )
            // What the last painted frame actually showed, not what the
            // state says it would show: the bottom row can change owner
            // with no pointer or modifier event behind it, and a press
            // arriving before the freed row has been repainted would open
            // a target no frame ever previewed.
            && activation_matches_preview(self.presented_link_target.as_ref(), &target)
        {
            if let Err(err) = open_url(&target) {
                // No launcher started, so no other window takes focus:
                // leave `self.modifiers` alone.
                warn!(?err, "open URL failed");
            } else {
                let (scheme, len) = target.log_fields();
                info!(?scheme, len, "opened OSC 8 hyperlink");
                // Clear Ctrl because the launched browser steals focus before release events
                // arrive, which would otherwise leave the Pointer icon stuck. Confined to
                // successful launches so modifiers stay consistent on failure.
                self.modifiers.remove(ModifiersState::CONTROL);
                self.update_mouse_cursor_icon();
            }
            return;
        }
        // A release takes the path its own press took; the mouse-mode /
        // Shift predicate can flip while the button is down.
        if state == ElementState::Released {
            match self.held_buttons.release(button) {
                Some(PressRouting::Grabbed) => {
                    // A click that never dragged leaves a stranded anchor.
                    if matches!(self.drag, DragState::Pending(..)) {
                        self.drag = DragState::Idle;
                    }
                    // Linux PRIMARY auto-copy; nothing selected after a
                    // no-drag click or a mid-hold grid invalidation.
                    if self.selection().is_some() {
                        self.auto_copy_selection_to_primary();
                    }
                }
                Some(PressRouting::Forwarded) => {
                    self.emit_mouse(Some(button), MouseAction::Release, cell);
                }
                // The client swallowed the press whole, or it landed before
                // this window had the pointer; the program saw no press.
                None => {}
            }
            return;
        }
        // Shift overrides program-side capture (xterm convention).
        let mouse_mode_active = self.shadow.mouse_protocol_active();
        let shift_held = self.modifiers.shift_key();
        if button == MouseButton::Left && should_start_selection(mouse_mode_active, shift_held) {
            self.drag = if self.modifiers.alt_key() {
                // Alt+press anchors a rectangle (macOS Option+drag; right-drag
                // is the xterm sibling below). A "rectangle word" has no
                // meaning, so the streak resets.
                self.click_streak.reset();
                DragState::Pending(cell, SelectionMode::Rectangle)
            } else {
                let streak = self.click_streak.record(Instant::now(), cell);
                let grid = self.shadow.screen();
                match streak {
                    2 => DragState::Selected(
                        Selection::word_at(grid, cell).unwrap_or_else(|| Selection::new(cell)),
                    ),
                    3 => DragState::Selected(
                        Selection::line_at(grid, cell.row).unwrap_or_else(|| Selection::new(cell)),
                    ),
                    // A click with no drag leaves nothing highlighted: the
                    // user clicked to dismiss, not to select.
                    _ => DragState::Pending(cell, SelectionMode::Linear),
                }
            };
            self.update_renderer_selection();
            self.held_buttons
                .press(MouseButton::Left, PressRouting::Grabbed);
            return;
        }
        // Forwarded to the program: no multi-click selection is being built.
        if button == MouseButton::Left {
            self.click_streak.reset();
        }
        // Right+Press starts a rectangle selection (xterm convention).
        if button == MouseButton::Right && should_start_selection(mouse_mode_active, shift_held) {
            self.drag = DragState::Selected(Selection::new_rectangle(cell));
            self.update_renderer_selection();
            self.held_buttons
                .press(MouseButton::Right, PressRouting::Grabbed);
            return;
        }
        // Middle-click pastes PRIMARY (xterm / urxvt / alacritty / kitty),
        // Shift overriding mouse mode. Selection stays untouched: xterm
        // defines paste as independent of selection state.
        if button == MouseButton::Middle && (!mouse_mode_active || shift_held) {
            self.paste_from_primary();
            return;
        }
        // See `forwarded_press_dismisses_selection` for why Middle is
        // excluded.
        if forwarded_press_dismisses_selection(button)
            && matches!(self.drag, DragState::Selected(_))
        {
            self.drag = DragState::Idle;
            self.update_renderer_selection();
        }
        self.held_buttons.press(button, PressRouting::Forwarded);
        self.emit_mouse(Some(button), MouseAction::Press, cell);
    }

    fn on_wheel(&mut self, delta: MouseScrollDelta) {
        let Some(cell) = self.cursor_cell else { return };
        // macOS delivers inertia as ordinary MouseWheel events and winit
        // 0.30 cannot flag them (`wheel_starts_new_stream`), so without
        // the latch a Ctrl press landing during leftover inertia would
        // turn an in-flight scroll into a font zoom.
        let now = Instant::now();
        let gap = self.last_wheel_at.map(|t| now.saturating_duration_since(t));
        self.last_wheel_at = Some(now);
        if wheel_starts_new_stream(gap) {
            self.wheel_zoom_latched = self.modifiers.control_key();
        }
        // Ctrl+wheel zoom (kitty, alacritty, browsers) preempts the program
        // even on alt-screen + mouse-mode, so a tmux user can still zoom;
        // Shift doesn't gate it. The latch keeps zoom tied to the stream's
        // opening intent.
        if self.wheel_zoom_latched && self.modifiers.control_key() {
            let dpx = wheel_zoom_delta_px(delta);
            if dpx != 0.0 {
                self.adjust_font_size(dpx);
            }
            return;
        }
        let mouse_mode_active = self.shadow.mouse_protocol_active();
        let on_alt = self.shadow.alt_screen();
        // Shift+wheel escapes a mouse-grabbing program (htop, mc,
        // vim+mouse) to page through history, as kitty / wezterm do.
        let force_scroll = self.modifiers.shift_key() && !on_alt;
        let encoding = if force_scroll {
            WheelEncoding::Scroll
        } else {
            wheel_encoding_for(mouse_mode_active, on_alt)
        };
        match encoding {
            WheelEncoding::MouseButtons => {
                for button in wheel_buttons(delta) {
                    self.emit_mouse(Some(button), MouseAction::Press, cell);
                }
            }
            WheelEncoding::ArrowKeys => {
                let bytes = wheel_arrow_bytes(delta, self.modifiers.shift_key());
                if !bytes.is_empty() {
                    self.send_input(&InputMsg::KeyBytes(bytes));
                }
            }
            WheelEncoding::Scroll => {
                // Wheel rolled up = scroll back into history.
                let shift = self.modifiers.shift_key();
                if shift {
                    // One tick = one half-page (kitty's scroll_half_page): a
                    // half-page per trackpad pixel is unusable.
                    let ticks = wheel_y_ticks(delta);
                    if ticks == 0 {
                        return;
                    }
                    let step = if ticks > 0 {
                        felis_client_core::action::ScrollStep::HalfPageUp
                    } else {
                        felis_client_core::action::ScrollStep::HalfPageDown
                    };
                    for _ in 0..ticks.unsigned_abs() {
                        self.apply_scroll(step);
                    }
                } else {
                    let cell_h = self
                        .surface
                        .as_ref()
                        .map_or(20.0, |s| f64::from(s.renderer.cell_metrics().height));
                    let rows = wheel_pixels_to_rows(
                        delta,
                        cell_h,
                        self.scroll_multiplier,
                        &mut self.scroll_pixel_accum_y,
                    );
                    self.scroll_by_rows(rows);
                }
            }
        }
    }

    fn on_redraw(&mut self, event_loop: &ActiveEventLoop) {
        // The shadow's dirty bit fires on `ImageMsg::Complete`; the
        // renderer caches the atlas slot for later frames.
        let dirty_ids = self.image_shadow.take_dirty_images();
        self.settle_hover_target_for_frame();
        // Nothing is on screen until `render` returns; a frame that never
        // reaches the compositor must not leave a target armed.
        self.presented_link_target = None;
        let search_overlay = self.build_search_overlay();
        let confirm_overlay = self.build_confirm_overlay();
        let link_preview_overlay = self.build_link_preview_overlay();
        // The target this frame puts on screen, kept beside the overlay it
        // was built from so the click gate compares against what was
        // painted rather than against a hover read at press time.
        let painted_target = link_preview_overlay
            .as_ref()
            .and_then(|_| self.hover_target.clone());
        // Borrowed this late: the overlay builders need `&self` as a whole.
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let renderer = &mut surface.renderer;
        renderer.set_search_overlay(search_overlay);
        renderer.set_confirm_overlay(confirm_overlay);
        renderer.set_link_preview_overlay(link_preview_overlay);
        let outcome = renderer.render(
            self.shadow.screen(),
            &dirty_ids,
            // Not just the dirty batch: an atlas reset invalidates every
            // slot, and the renderer re-uploads the whole live set.
            &ShadowImages(&self.image_shadow),
            self.image_shadow.placements(),
            self.image_shadow.virtual_placements(),
            self.shadow.viewport(),
            self.shadow.viewport_max(),
        );
        // Whatever the outcome: the renderer took the rows into its
        // instance buffers before acquiring the surface.
        self.shadow.clear_damage();
        let painted = outcome.is_ok();
        let was_stalled = self.present_retry.stalled();
        if painted {
            self.redraw.painted();
            self.present_retry.presented();
        } else {
            let interval = refresh_interval(&surface.window);
            self.present_retry.unpresented(Instant::now(), interval);
        }
        if let Err(err) = outcome {
            match err {
                // Stale swapchain (compositor reconnect, mode change, DPI
                // switch): reconfigure and drop this frame; winit requests
                // another redraw.
                SurfaceError::Lost | SurfaceError::Outdated => {
                    renderer.reconfigure_surface();
                }
                // Not recoverable by retrying.
                SurfaceError::OutOfMemory => {
                    error!("surface out of memory; exiting");
                    event_loop.exit();
                }
                SurfaceError::Timeout if was_stalled => {
                    tracing::debug!("surface still cannot present; skipping frame");
                }
                SurfaceError::Timeout => {
                    warn!("surface cannot present; pausing frame pulls until it can");
                }
                other @ SurfaceError::Other(_) => warn!(?other, "render failed"),
            }
        }
        // Cells painted blank because the glyph atlas was full; rendering
        // is damage-driven, so nothing else will dirty those rows.
        let glyphs_incomplete = renderer.take_glyph_repaint_request();
        if glyphs_incomplete {
            self.redraw.request();
        }
        // A frame that ran out of atlas may have dropped glyphs from the
        // preview bar as readily as from the grid, and a target missing
        // its first characters reads as a different host. Arm only on a
        // frame that drew every glyph it was asked for.
        if painted && !glyphs_incomplete {
            self.presented_link_target = painted_target;
        }
        // Demand-driven emission (docs/explanation/rendering/pipeline.md):
        // once per presented frame, which the present paces at vsync, so
        // the daemon ships at most one coalesced diff per vsync.
        if self.pull.after_frame(painted) {
            self.send_input(&InputMsg::NextGridFrame);
            tracing::trace!(
                target: "felis::pull",
                sent = self.pull.sent(),
                "grid frame pull",
            );
        }
        // Last in the frame: the run's evidence is the frame just painted.
        self.smoke_step();
    }
}

/// A monitor that reports no rate falls back to 60 Hz.
fn refresh_interval(window: &winit::window::Window) -> Duration {
    let millihertz = window
        .current_monitor()
        .and_then(|monitor| monitor.refresh_rate_millihertz())
        .filter(|&mhz| mhz > 0)
        .unwrap_or(60_000);
    Duration::from_micros(1_000_000_000 / u64::from(millihertz))
}

/// Whether a `RedrawRequested` may paint. The source of the request is
/// deliberately not a parameter: a blink, an animation tick, a facet
/// push and a pending redraw are all the same hazard mid-cycle, which
/// is putting a `Scrolled` on screen without the rows that follow it.
const fn may_paint_grid(has_surface: bool, rehydrating: bool, cycle_open: bool) -> bool {
    has_surface && !rehydrating && !cycle_open
}

/// On Unix the raw `OsStr`, so a non-UTF-8 filename reaches the shell
/// intact; Windows (UTF-16) has no byte-exact path, so lossy conversion
/// is the floor there.
#[cfg(unix)]
fn dropped_path_bytes(path: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn dropped_path_bytes(path: &std::path::Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(all(test, unix))]
mod drop_tests {
    use super::dropped_path_bytes;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    #[test]
    fn plain_utf8_path_round_trips() {
        assert_eq!(
            dropped_path_bytes(Path::new("/tmp/a b.txt")),
            b"/tmp/a b.txt"
        );
    }

    #[test]
    fn non_utf8_path_bytes_survive_unmangled() {
        // Lossy conversion would turn 0xFF into U+FFFD, breaking the file path.
        let raw = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff.bin"));
        assert_eq!(dropped_path_bytes(raw), b"/tmp/\xff.bin");
    }
}

#[cfg(test)]
mod paint_gate_tests {
    use super::may_paint_grid;
    use felis_client_core::pull::PullScheduler;
    use felis_protocol::messages::{AttentionSource, GridMsg, ScrollDirection};

    fn painted(pull: &PullScheduler) -> bool {
        may_paint_grid(true, false, pull.cycle_open())
    }

    /// A cycle that has begun arriving paints nothing, whatever asks
    /// for the redraw, until its marker closes it.
    #[test]
    fn no_redraw_source_paints_a_cycle_before_its_marker() {
        let mut pull = PullScheduler::new(true);
        assert!(pull.after_frame(true));
        assert!(painted(&pull), "an idle window paints");

        pull.on_frame(&GridMsg::Scrolled {
            region_top: 0,
            region_bottom: 23,
            n_rows: 5,
            direction: ScrollDirection::Up,
        });
        assert!(
            !painted(&pull),
            "the scroll alone must not reach the screen"
        );

        pull.on_frame(&GridMsg::Attention {
            source: AttentionSource::Bell,
        });
        assert!(!painted(&pull), "a facet push cannot paint the grid");
        assert!(!painted(&pull), "nor can a blink or animation tick");

        pull.on_frame(&GridMsg::CycleEnd);
        assert!(painted(&pull), "the marker releases the paint");
    }

    /// A transport that dies mid-cycle leaves the `Scrolled` it already
    /// applied on the shadow with no rows behind it. Replacing the
    /// scheduler on the reconnect must not paint that screen; only the
    /// replacement's burst may, and the disconnected title rides
    /// `set_title`, not a paint.
    #[test]
    fn a_cycle_cut_short_by_a_disconnect_stays_gated_until_rehydrate() {
        let mut lost = PullScheduler::new(true);
        assert!(lost.after_frame(true));
        lost.on_frame(&GridMsg::Scrolled {
            region_top: 0,
            region_bottom: 23,
            n_rows: 5,
            direction: ScrollDirection::Up,
        });
        assert!(!painted(&lost), "the rows of this cycle never arrived");

        // `install_new_session` builds the landing's scheduler here.
        let mut landed = PullScheduler::new(true).holding_gate(lost.cycle_open());
        assert!(
            !painted(&landed),
            "a redraw before the burst must not paint the half-cycle"
        );

        landed.on_frame(&GridMsg::RehydrateBegin);
        landed.on_frame(&GridMsg::RehydrateEnd);
        assert!(painted(&landed), "the burst restated the whole screen");
    }

    /// The rehydrate burst keeps its own boundary; the two gates are
    /// independent.
    #[test]
    fn a_rehydrating_shadow_paints_nothing_even_with_no_cycle_open() {
        assert!(!may_paint_grid(true, true, false));
        assert!(!may_paint_grid(false, false, false));
    }
}

use std::time::Duration;

use felis_client_core::config::Backdrop;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::window::{CursorIcon, Fullscreen, Theme, UserAttentionType, Window};

/// Every effect `App` has on its window once the window exists: the
/// winit window in a window build, a recorder in tests.
pub(crate) trait WindowHost {
    fn set_title(&self, title: &str);
    fn set_cursor(&self, icon: CursorIcon);
    fn request_attention(&self);
    fn request_redraw(&self);
    fn set_ime_cursor_area(&self, position: PhysicalPosition<f64>, size: PhysicalSize<u32>);
    fn inner_size(&self) -> PhysicalSize<u32>;
    /// `None` when the platform reports no preference.
    fn prefers_dark(&self) -> Option<bool>;
    fn toggle_fullscreen(&self);
    fn set_decorations(&self, decorations: bool);
    fn set_option_as_alt(&self, option_as_alt: bool);
    fn apply_backdrop(&self, backdrop: Backdrop, transparent: bool);
    /// See `macos_window`; a no-op elsewhere.
    fn repair_translucent_chrome(&self);
    fn refresh_interval(&self) -> Duration;
}

impl WindowHost for Window {
    fn set_title(&self, title: &str) {
        Self::set_title(self, title);
    }

    fn set_cursor(&self, icon: CursorIcon) {
        Self::set_cursor(self, icon);
    }

    fn request_attention(&self) {
        self.request_user_attention(Some(UserAttentionType::Informational));
    }

    fn request_redraw(&self) {
        Self::request_redraw(self);
    }

    fn set_ime_cursor_area(&self, position: PhysicalPosition<f64>, size: PhysicalSize<u32>) {
        Self::set_ime_cursor_area(self, position, size);
    }

    fn inner_size(&self) -> PhysicalSize<u32> {
        Self::inner_size(self)
    }

    fn prefers_dark(&self) -> Option<bool> {
        self.theme().map(|theme| theme == Theme::Dark)
    }

    fn toggle_fullscreen(&self) {
        let next = if self.fullscreen().is_some() {
            None
        } else {
            Some(Fullscreen::Borderless(None))
        };
        self.set_fullscreen(next);
    }

    fn set_decorations(&self, decorations: bool) {
        Self::set_decorations(self, decorations);
    }

    fn set_option_as_alt(&self, option_as_alt: bool) {
        crate::apply_option_as_alt(self, option_as_alt);
    }

    fn apply_backdrop(&self, backdrop: Backdrop, transparent: bool) {
        crate::apply_window_backdrop(self, backdrop, transparent);
    }

    fn repair_translucent_chrome(&self) {
        #[cfg(target_os = "macos")]
        crate::macos_window::fix_translucent_layer_stack(self);
    }

    /// A monitor that reports no rate falls back to 60 Hz.
    fn refresh_interval(&self) -> Duration {
        let millihertz = self
            .current_monitor()
            .and_then(|monitor| monitor.refresh_rate_millihertz())
            .filter(|&mhz| mhz > 0)
            .unwrap_or(60_000);
        Duration::from_micros(1_000_000_000 / u64::from(millihertz))
    }
}

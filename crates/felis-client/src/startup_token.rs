//! The launcher's startup-notification token: Wayland's
//! `XDG_ACTIVATION_TOKEN` or X11's `DESKTOP_STARTUP_ID`, which lets the
//! first window take focus under a compositor that prevents focus
//! stealing.

use winit::{event_loop::ActiveEventLoop, window::WindowAttributes};

#[cfg(target_os = "linux")]
const WAYLAND_VAR: &str = "XDG_ACTIVATION_TOKEN";
#[cfg(target_os = "linux")]
const X11_VAR: &str = "DESKTOP_STARTUP_ID";

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct StartupToken {
    wayland: Option<String>,
    x11: Option<String>,
}

impl StartupToken {
    /// Removes both, so a child does not present this window's token.
    ///
    /// # Safety
    ///
    /// No other thread may exist (`remove_var` races any env read).
    #[allow(unsafe_code)]
    pub(crate) unsafe fn take_from_env() -> Self {
        #[cfg(target_os = "linux")]
        {
            let token = Self {
                wayland: std::env::var(WAYLAND_VAR).ok(),
                x11: std::env::var(X11_VAR).ok(),
            };
            // SAFETY: the caller guarantees this is the only thread.
            #[allow(clippy::disallowed_methods)]
            unsafe {
                std::env::remove_var(WAYLAND_VAR);
                std::env::remove_var(X11_VAR);
            }
            token
        }
        #[cfg(not(target_os = "linux"))]
        Self::default()
    }

    /// Hands the token for `event_loop`'s backend to the window about to
    /// be created; the other backend's token is dropped unused.
    pub(crate) fn apply(
        self,
        event_loop: &ActiveEventLoop,
        attrs: WindowAttributes,
    ) -> WindowAttributes {
        #[cfg(target_os = "linux")]
        {
            use winit::platform::{
                startup_notify::WindowAttributesExtStartupNotify as _,
                wayland::ActiveEventLoopExtWayland as _,
            };
            let raw = if event_loop.is_wayland() {
                self.wayland
            } else {
                self.x11
            };
            match raw {
                Some(raw) => {
                    attrs.with_activation_token(winit::window::ActivationToken::from_raw(raw))
                }
                None => attrs,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = event_loop;
            drop(self);
            attrs
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    const FIXTURE: &str =
        "startup_token::tests::taking_the_token_clears_it_from_the_process_environment";

    /// The re-exec gives the fixture a launcher's environment without
    /// `set_var`, which this workspace forbids.
    #[test]
    fn a_launched_window_keeps_the_token_out_of_what_it_spawns() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "--ignored", "--test-threads=1", FIXTURE])
            .env(WAYLAND_VAR, "wayland-token")
            .env(X11_VAR, "x11-token")
            .output()
            .expect("re-run this test binary for the fixture");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "the fixture did not run and pass: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    #[ignore = "fixture: run by a_launched_window_keeps_the_token_out_of_what_it_spawns"]
    #[allow(unsafe_code)]
    fn taking_the_token_clears_it_from_the_process_environment() {
        // SAFETY: under `--test-threads=1` the harness's main thread only
        // joins this one, and this test starts no thread of its own.
        let token = unsafe { StartupToken::take_from_env() };
        assert_eq!(
            token,
            StartupToken {
                wayland: Some("wayland-token".into()),
                x11: Some("x11-token".into()),
            }
        );
        for var in [WAYLAND_VAR, X11_VAR] {
            assert_eq!(
                std::env::var_os(var),
                None,
                "{var} must not reach a child this client spawns"
            );
        }
    }
}

//! macOS `CoreAnimation` window-chrome fixes for translucent windows (`docs/reference/config.md`).
//!
//! Raises `CAMetalLayer` above `NSVisualEffectView` via `zPosition` and applies `cornerRadius`
//! directly to both root and Metal layers so window corners clip correctly.

#![allow(unsafe_code)]

use objc2::runtime::NSObjectProtocol;
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_app_kit::NSView;
use objc2_quartz_core::{CALayer, CAMetalLayer};
use tracing::{debug, warn};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// 10 pt is the observed radius of a titled window from Big Sur through
/// Sequoia; newer releases report their own value through the probe
/// (Tahoe says 12).
const FALLBACK_CORNER_RADIUS: f64 = 10.0;

/// Call after the renderer is built (earlier there is no Metal sublayer
/// to raise) and again on every resize.
///
/// Failures degrade to warnings: a missed fix is cosmetic and the user can
/// escape it with `window.opacity = 1.0`.
pub(crate) fn fix_translucent_layer_stack(window: &Window) {
    let Some(_mtm) = MainThreadMarker::new() else {
        warn!("translucent-window layer fix skipped: not on the main thread");
        return;
    };
    let Ok(RawWindowHandle::AppKit(handle)) = window.window_handle().map(|h| h.as_raw()) else {
        warn!("translucent-window layer fix skipped: no AppKit window handle");
        return;
    };
    let ns_view = handle.ns_view;
    // SAFETY: `ns_view` is the pointer winit hands out via
    // `AppKitWindowHandle`: a valid `NSView` for as long as `window`
    // lives, and we hold `&Window` across this whole call. The
    // reference never escapes, and we are on the main thread (checked
    // above), matching AppKit's threading contract.
    let view: &NSView = unsafe { ns_view.cast().as_ref() };

    let Some(root) = view.layer() else {
        warn!("translucent-window layer fix skipped: NSView has no layer");
        return;
    };

    let radius = window_corner_radius(view);
    raise_and_round_metal_sublayer(&root, radius);

    // Rounds the vibrancy backdrop but NOT the Metal layer, whose drawable
    // bypasses the mask (rounded above, directly on the layer).
    root.setCornerRadius(radius);
    root.setMasksToBounds(true);
}

/// The class check is `isKindOfClass`-based (`downcast_ref`), not name
/// equality, because wgpu installs a private `CAMetalLayer` subclass
/// (`WgpuObserverLayer@…`) to track bounds.
fn raise_and_round_metal_sublayer(root: &CALayer, radius: f64) {
    // SAFETY: `sublayers` is a read-only property getter; the returned
    // array is retained by `Retained` and not mutated while we iterate.
    let Some(sublayers) = (unsafe { root.sublayers() }) else {
        warn!("translucent-window layer fix: root layer has no sublayers");
        return;
    };
    let mut raised = false;
    for sublayer in &sublayers {
        if let Some(metal) = sublayer.downcast_ref::<CAMetalLayer>() {
            metal.setZPosition(1.0);
            metal.setCornerRadius(radius);
            metal.setMasksToBounds(true);
            raised = true;
        }
    }
    if !raised {
        warn!(
            "translucent-window layer fix: no CAMetalLayer sublayer found; \
             the blur backdrop may cover the terminal"
        );
    }
}

/// There is no public `AppKit` API for the window's corner radius, and it
/// has changed across OS releases (Big Sur, Tahoe), so a per-version table
/// would drift on every macOS update. The guarded private-selector read
/// degrades to [`FALLBACK_CORNER_RADIUS`] if Apple removes the selector.
fn window_corner_radius(view: &NSView) -> f64 {
    let Some(window) = view.window() else {
        return FALLBACK_CORNER_RADIUS;
    };
    if window.respondsToSelector(sel!(_cornerRadius)) {
        // SAFETY: the selector exists (guard above) and `_cornerRadius`
        // is a zero-argument CGFloat getter on NSWindow (observed
        // stable since 10.x; rio ships the same probe).
        let radius: f64 = unsafe { msg_send![&*window, _cornerRadius] };
        if radius.is_finite() && radius >= 0.0 {
            debug!(radius, "window corner radius probed");
            return radius;
        }
    }
    debug!(
        fallback = FALLBACK_CORNER_RADIUS,
        "window corner radius probe unavailable; using fallback"
    );
    FALLBACK_CORNER_RADIUS
}

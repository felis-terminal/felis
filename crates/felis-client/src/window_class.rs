//! Windows: refuse a `--class` that names a window class this process
//! cannot register. winit ignores a failed `RegisterClassExW` and still
//! hands the name to `CreateWindowExW`, which would then build a window
//! of that other class, or none.

use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassInfoExW, WNDCLASSEXW};

/// Registered by winit while the event loop is built, after this check
/// has to run, so no lookup can find it yet.
const WINIT_EVENT_TARGET: &str = "Winit Thread Event Target";

/// Why `name` cannot be this window's class, or `None` when it can.
/// Runs before the client dials the daemon, so a refusal leaves no
/// session behind.
pub(crate) fn collision(name: &str) -> Option<String> {
    if same_class_name(name, WINIT_EVENT_TARGET) {
        return Some(format!(
            "`{name}` is the class winit registers for its own event loop"
        ));
    }
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: a null name asks for the module that created this process
    // and keeps no reference.
    #[allow(unsafe_code)] // against the workspace `unsafe_code = "deny"`.
    let own_module = unsafe { GetModuleHandleW(std::ptr::null()) };
    // winit registers under the executable's own image base, which is
    // `own_module`; a null module is where the system classes live.
    for module in [own_module, std::ptr::null_mut()] {
        // SAFETY: `wide` is NUL-terminated and outlives the call;
        // `info` is a writable WNDCLASSEXW with `cbSize` set, as the
        // call requires. Neither pointer is retained.
        #[allow(unsafe_code)] // against the workspace `unsafe_code = "deny"`.
        let found = unsafe {
            let mut info: WNDCLASSEXW = std::mem::zeroed();
            info.cbSize = u32::try_from(size_of::<WNDCLASSEXW>()).unwrap_or(u32::MAX);
            GetClassInfoExW(module, wide.as_ptr(), &raw mut info)
        };
        if found != 0 {
            return Some(format!(
                "`{name}` is already a registered window class (a system class such as `Button`, \
                 or one this process registers)"
            ));
        }
    }
    None
}

/// Win32 compares class names case-insensitively. Both folds are
/// checked, so a name that matches under either one is refused: an
/// extra refusal costs a rename, a missed one a broken window.
fn same_class_name(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_uppercase)
        .eq(b.chars().flat_map(char::to_uppercase))
        || a.chars()
            .flat_map(char::to_lowercase)
            .eq(b.chars().flat_map(char::to_lowercase))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{DefWindowProcW, RegisterClassExW};

    #[test]
    fn a_system_class_is_refused_in_any_casing() {
        for name in ["Button", "BUTTON", "button", "Edit", "Static"] {
            assert!(collision(name).is_some(), "{name}");
        }
    }

    #[test]
    fn winit_event_target_is_refused_in_any_casing() {
        for name in [
            "Winit Thread Event Target",
            "winit thread event target",
            "WINIT THREAD EVENT TARGET",
            "wInIt ThReAd EvEnT tArGeT",
        ] {
            let reason = collision(name).unwrap_or_else(|| panic!("{name} must be refused"));
            assert!(reason.contains("event loop"), "{reason}");
        }
    }

    #[test]
    fn a_class_this_process_registered_is_refused() {
        let name = format!("felis-test-class-{}", std::process::id());
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        #[allow(unsafe_code)]
        // SAFETY: the class record points at a NUL-terminated name that
        // outlives the registration call, and at the system default
        // window procedure.
        let atom = unsafe {
            let mut class: WNDCLASSEXW = std::mem::zeroed();
            class.cbSize = u32::try_from(size_of::<WNDCLASSEXW>()).unwrap();
            class.lpfnWndProc = Some(DefWindowProcW);
            class.hInstance = GetModuleHandleW(std::ptr::null());
            class.lpszClassName = wide.as_ptr();
            RegisterClassExW(&raw const class)
        };
        assert_ne!(atom, 0, "registering {name}");
        assert!(collision(&name).is_some());
        assert!(collision(&name.to_uppercase()).is_some());
    }

    #[test]
    fn a_fresh_name_is_accepted() {
        let name = format!("felis-unregistered-{}", std::process::id());
        assert_eq!(collision(&name), None);
        assert_eq!(collision("felis"), None);
    }
}

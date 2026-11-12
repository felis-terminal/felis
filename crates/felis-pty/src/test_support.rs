use crate::Size;

pub(crate) fn small_size() -> Size {
    Size {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// From `ComSpec` with the canonical location as a fallback, so the
/// spawn resolves under a stripped-down environment whose `PATH`
/// lacks `System32`.
#[cfg(windows)]
pub(crate) fn comspec() -> std::ffi::OsString {
    std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into())
}

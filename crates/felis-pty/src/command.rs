//! Platform-neutral child-command description and PTY geometry.
//!
//! [`Command`] is a plain data bag rather than a `std::process::Command` wrapper
//! so `ConPTY` can call `CreateProcessW` directly without backend coupling.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

/// PTY viewport geometry, in cells and pixels.
///
/// Pixel dimensions feed `TIOCSWINSZ` so cell-size-aware protocols
/// (Kitty graphics' `CSI 14 t` report) see honest values; `ConPTY` has
/// no pixel notion and ignores them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
    /// Viewport width in pixels (0 when unknown).
    pub pixel_width: u16,
    /// Viewport height in pixels (0 when unknown).
    pub pixel_height: u16,
}

/// Description of the child to spawn into the PTY.
///
/// [`Command::new`] snapshots the environment eagerly so inherit-and-denylist
/// policies act on a fixed map immune to subsequent process env mutations.
#[derive(Debug, Clone)]
pub struct Command {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) cwd: Option<PathBuf>,
    /// `BTreeMap`: `CreateProcessW` documents a sorted environment
    /// block, and tests see one ordering on every platform.
    pub(crate) env: BTreeMap<OsString, OsString>,
}

/// Case-fold Windows environment variable names to uppercase.
///
/// Windows environment keys are case-insensitive. Folding avoids collision
/// and missed removals when matching casing like `Path` against `PATH`.
#[cfg(windows)]
pub(crate) fn env_key(key: &OsStr) -> OsString {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    // ASCII fold only, matching the invariant-locale uppercasing Win32
    // uses to order and compare the environment block.
    let folded: Vec<u16> = key
        .encode_wide()
        .map(|unit| match unit {
            0x61..=0x7A => unit - 0x20,
            other => other,
        })
        .collect();
    OsString::from_wide(&folded)
}

#[cfg(not(windows))]
pub(crate) fn env_key(key: &OsStr) -> OsString {
    key.to_os_string()
}

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_os_string(),
            args: Vec::new(),
            cwd: None,
            env: std::env::vars_os().map(|(k, v)| (env_key(&k), v)).collect(),
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    pub fn cwd(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.env
            .insert(env_key(key.as_ref()), val.as_ref().to_os_string());
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.env.remove(&env_key(key.as_ref()));
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.env.clear();
        self
    }

    /// Replace the inherited environment snapshot entirely with `base`.
    ///
    /// Never an overlay: completely displaces the parent's environment to avoid
    /// leaking stale daemon variables across SSH hops or detached sessions.
    pub fn env_base<I, K, V>(&mut self, base: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        self.env.clear();
        for (key, value) in base {
            let (Some(key), Some(value)) =
                (env_from_bytes(key.as_ref()), env_from_bytes(value.as_ref()))
            else {
                continue;
            };
            self.env.insert(env_key(&key), value);
        }
        self
    }

    pub fn get_envs(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.env.iter().map(|(k, v)| (k.as_os_str(), v.as_os_str()))
    }

    #[must_use]
    pub fn get_cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    #[must_use]
    pub fn get_program(&self) -> &OsStr {
        &self.program
    }
}

/// One environment string as raw platform spawn bytes (Unix bytes, Windows
/// UTF-16LE). Preserves arbitrary non-UTF-8 bytes so paths still resolve.
#[cfg(unix)]
#[must_use]
pub fn env_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
#[must_use]
pub fn env_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

/// The inverse of [`env_bytes`]. `None` when `bytes` cannot be a value
/// of this platform: on Windows an odd length, half a `u16` code unit,
/// which would otherwise silently lose its last byte.
#[cfg(unix)]
#[must_use]
#[expect(
    clippy::unnecessary_wraps,
    reason = "one signature with the Windows branch, where an odd length is not a value"
)]
pub fn env_from_bytes(bytes: &[u8]) -> Option<OsString> {
    use std::os::unix::ffi::OsStringExt;
    Some(OsString::from_vec(bytes.to_vec()))
}

#[cfg(windows)]
#[must_use]
pub fn env_from_bytes(bytes: &[u8]) -> Option<OsString> {
    use std::os::windows::ffi::OsStringExt;
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    Some(OsString::from_wide(&units))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `new` snapshots the parent env up front and clear/remove/override
    /// shape it; the key is `PATH` on every platform (folded from the
    /// OS's `Path` on Windows).
    #[test]
    fn env_snapshot_then_narrowing() {
        let cmd = Command::new("true");
        assert!(cmd.env.contains_key(OsStr::new("PATH")));

        let mut cmd = Command::new("true");
        cmd.env_clear();
        assert!(cmd.env.is_empty());
        cmd.env("A", "1").env("B", "2");
        cmd.env_remove("A");
        assert_eq!(cmd.env.get(OsStr::new("B")).unwrap(), "2");
        assert!(!cmd.env.contains_key(OsStr::new("A")));
    }

    /// Windows env names are case-insensitive: a mixed-case override
    /// replaces the inherited entry and a mixed-case `env_remove` finds it.
    #[cfg(windows)]
    #[test]
    fn env_keys_are_case_insensitive_on_windows() {
        let mut cmd = Command::new("cmd.exe");
        cmd.env_clear();
        cmd.env("Path", "a");
        cmd.env("PATH", "b");
        assert_eq!(cmd.env.len(), 1);
        assert_eq!(cmd.env.get(OsStr::new("PATH")).unwrap(), "b");
        cmd.env_remove("path");
        assert!(cmd.env.is_empty());
    }

    /// Byte literals would leave the base empty on Windows, where an
    /// entry is UTF-16LE code units and `env_base` skips anything else.
    fn entry(name: &str, value: &str) -> (Vec<u8>, Vec<u8>) {
        (env_bytes(OsStr::new(name)), env_bytes(OsStr::new(value)))
    }

    /// A supplied base replaces the inherited snapshot instead of
    /// layering over it.
    #[test]
    fn an_env_base_replaces_the_inherited_snapshot() {
        let mut cmd = Command::new("true");
        assert!(cmd.env.contains_key(OsStr::new("PATH")));
        cmd.env_base([entry("BASE_ONLY", "1"), entry("PATH", "/nowhere")]);
        assert_eq!(cmd.env.get(OsStr::new("BASE_ONLY")).unwrap(), "1");
        assert_eq!(cmd.env.get(OsStr::new("PATH")).unwrap(), "/nowhere");
        assert_eq!(cmd.env.len(), 2, "nothing inherited survives");
    }

    /// An override applied after a base replaces the base entry rather
    /// than duplicating it.
    #[test]
    fn overrides_apply_over_a_base() {
        let mut cmd = Command::new("true");
        cmd.env_base([entry("TERM", "dumb")]);
        cmd.env("TERM", "xterm-felis");
        assert_eq!(cmd.env.get(OsStr::new("TERM")).unwrap(), "xterm-felis");
        assert_eq!(cmd.env.len(), 1);
    }

    /// The round trip carries a non-UTF-8 value, which is what the
    /// platform-bytes encoding exists for.
    #[test]
    fn platform_bytes_round_trip() {
        let value = env_from_bytes(b"/tmp/agent").unwrap();
        assert_eq!(env_bytes(&value), b"/tmp/agent");
        #[cfg(unix)]
        {
            let raw = &[0x2f, 0xff, 0xfe][..];
            assert_eq!(env_bytes(&env_from_bytes(raw).unwrap()), raw);
        }
    }

    /// Half a `u16` code unit is not a Windows value; silent truncation
    /// would hand the child a different path than the captured one.
    #[cfg(windows)]
    #[test]
    fn an_odd_byte_count_is_not_a_windows_value() {
        assert!(env_from_bytes(&[0x41]).is_none());
        assert!(env_from_bytes(&[0x41, 0x00]).is_some());
    }
}

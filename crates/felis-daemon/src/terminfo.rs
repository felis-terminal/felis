//! The compiled `xterm-felis` entry shipped beside this daemon, handed
//! to PTY children through `TERMINFO_DIRS` (docs/reference/terminal-identity.md).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use felis_pty::Command;

use crate::DEFAULT_TERM;

const TERMINFO_DIRS: &str = "TERMINFO_DIRS";

pub(crate) fn shipped_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    if !cfg!(unix) {
        return None;
    }
    DIR.get_or_init(|| {
        // Canonical, because macOS reports the path the daemon was
        // launched by: through a Nix profile link, its prefix would be
        // the profile, whose share/terminfo is every package's.
        let exe = std::fs::canonicalize(std::env::current_exe().ok()?).ok()?;
        find_for_exe(&exe)
    })
    .as_deref()
}

fn find_for_exe(exe: &Path) -> Option<PathBuf> {
    let prefix = exe.parent()?.parent()?;
    candidates(prefix).into_iter().find(|dir| has_entry(dir))
}

fn candidates(prefix: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(2);
    if cfg!(target_os = "macos") {
        dirs.push(prefix.join("Resources").join("terminfo"));
    }
    dirs.push(prefix.join("share").join("terminfo"));
    dirs
}

fn has_entry(dir: &Path) -> bool {
    let Some(first) = DEFAULT_TERM.chars().next() else {
        return false;
    };
    [first.to_string(), format!("{:x}", u32::from(first))]
        .iter()
        .any(|leaf| dir.join(leaf).join(DEFAULT_TERM).is_file())
}

pub(crate) fn list_in(cmd: &mut Command, dir: &Path) {
    let current = cmd
        .get_envs()
        .find(|(key, _)| *key == TERMINFO_DIRS)
        .map(|(_, value)| value.to_os_string());
    if let Some(merged) = prepend(dir, current.as_deref()) {
        cmd.env(TERMINFO_DIRS, merged);
    }
}

/// `current` with `dir` in front, or `None` when nothing should change.
///
/// An unset `current` gains a trailing empty element, ncurses' spelling
/// for its compiled-in default directories; without it the host's own
/// database would stop being searched.
fn prepend(dir: &Path, current: Option<&OsStr>) -> Option<OsString> {
    // `TERMINFO_DIRS` has no quoting, so such a directory cannot be listed.
    if dir.as_os_str().as_encoded_bytes().contains(&b':') {
        return None;
    }
    if let Some(current) = current
        && std::env::split_paths(current).any(|listed| listed == dir)
    {
        return None;
    }
    let mut merged = dir.as_os_str().to_os_string();
    merged.push(":");
    if let Some(current) = current {
        merged.push(current);
    }
    Some(merged)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn install_entry(dir: &Path, leaf: &str) -> std::io::Result<()> {
        let leaf_dir = dir.join(leaf);
        std::fs::create_dir_all(&leaf_dir)?;
        std::fs::write(leaf_dir.join(DEFAULT_TERM), b"")
    }

    #[test]
    fn a_prefix_install_resolves_its_share_terminfo() {
        let root = tempfile::tempdir().unwrap();
        install_entry(&root.path().join("share/terminfo"), "x").unwrap();
        assert_eq!(
            find_for_exe(&root.path().join("bin/felis-daemon")),
            Some(root.path().join("share/terminfo")),
        );
    }

    #[test]
    fn the_hex_leaf_layout_counts_as_an_entry() {
        let root = tempfile::tempdir().unwrap();
        install_entry(&root.path().join("share/terminfo"), "78").unwrap();
        assert_eq!(
            find_for_exe(&root.path().join("bin/felis-daemon")),
            Some(root.path().join("share/terminfo")),
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_app_bundle_resolves_its_resources_terminfo() {
        let root = tempfile::tempdir().unwrap();
        let contents = root.path().join("felis.app/Contents");
        install_entry(&contents.join("Resources/terminfo"), "78").unwrap();
        assert_eq!(
            find_for_exe(&contents.join("MacOS/felis-daemon")),
            Some(contents.join("Resources/terminfo")),
        );
    }

    #[test]
    fn a_directory_without_the_entry_is_not_offered() {
        let root = tempfile::tempdir().unwrap();
        install_entry(&root.path().join("share/terminfo"), "v").unwrap();
        assert_eq!(find_for_exe(&root.path().join("bin/felis-daemon")), None);
    }

    #[test]
    fn a_tree_without_terminfo_resolves_nothing() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(find_for_exe(&root.path().join("debug/felis-daemon")), None);
    }

    #[test]
    fn prepending_to_an_unset_list_keeps_the_default_directories() {
        assert_eq!(
            prepend(Path::new("/opt/felis/share/terminfo"), None),
            Some(OsString::from("/opt/felis/share/terminfo:")),
        );
    }

    #[test]
    fn prepending_keeps_the_existing_list_behind_the_directory() {
        assert_eq!(
            prepend(
                Path::new("/opt/felis/share/terminfo"),
                Some(OsStr::new("/usr/local/share/terminfo:")),
            ),
            Some(OsString::from(
                "/opt/felis/share/terminfo:/usr/local/share/terminfo:"
            )),
        );
    }

    #[test]
    fn prepending_to_an_empty_list_yields_the_directory_and_defaults() {
        assert_eq!(
            prepend(Path::new("/opt/felis/share/terminfo"), Some(OsStr::new(""))),
            Some(OsString::from("/opt/felis/share/terminfo:")),
        );
    }

    #[test]
    fn an_already_listed_directory_changes_nothing() {
        assert_eq!(
            prepend(
                Path::new("/opt/felis/share/terminfo"),
                Some(OsStr::new(
                    "/home/u/.nix-profile/share/terminfo:/opt/felis/share/terminfo:"
                )),
            ),
            None,
        );
    }

    #[test]
    fn a_directory_containing_the_separator_is_never_listed() {
        assert_eq!(prepend(Path::new("/Users/u/My:Apps/terminfo"), None), None);
    }
}

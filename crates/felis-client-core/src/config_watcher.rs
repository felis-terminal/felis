//! Polls `config.toml` and notifies the frontend when it changes.
//!
//! Polling avoids varied editor save quirks. The [`Stamp`] pairs mtime with the
//! resolved target path to detect Nix/home-manager symlink updates where store
//! mtimes stay frozen at the epoch.

use std::{path::PathBuf, time::Duration};

use tokio::{runtime::Handle, time::sleep};
use tracing::warn;

const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// `notify` returns `false` when the frontend's event loop has closed;
/// the task then exits.
pub fn spawn<F>(runtime: &Handle, path: PathBuf, notify: F)
where
    F: Fn() -> bool + Send + 'static,
{
    runtime.spawn(async move { run(path, notify).await });
}

async fn run<F>(path: PathBuf, notify: F)
where
    F: Fn() -> bool + Send + 'static,
{
    let mut last = stamp(&path);
    loop {
        sleep(POLL_INTERVAL).await;
        let current = stamp(&path);
        let reload = should_reload(&last, &current);
        last = current;
        if reload && !notify() {
            warn!("config watcher: frontend gone, exiting");
            return;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stamp {
    /// `None` when the path is absent or a dangling symlink.
    target: Option<PathBuf>,
    /// Followed-target mtime since the UNIX epoch.
    mtime: Option<Duration>,
}

fn stamp(path: &std::path::Path) -> Stamp {
    let target = std::fs::canonicalize(path).ok();
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
    Stamp { target, mtime }
}

/// An unresolved current target (absent or dangling symlink, as
/// mid-`home-manager switch`) never reloads: reloading a deleted config
/// would clobber live settings with defaults.
#[must_use]
pub fn should_reload(prev: &Stamp, current: &Stamp) -> bool {
    if current.target.is_none() {
        return false;
    }
    prev != current
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(target: Option<&str>, mtime_secs: Option<u64>) -> Stamp {
        Stamp {
            target: target.map(PathBuf::from),
            mtime: mtime_secs.map(Duration::from_secs),
        }
    }

    #[test]
    fn should_reload_fires_when_file_appears() {
        assert!(should_reload(
            &st(None, None),
            &st(Some("/nix/store/aaa-config.toml"), Some(1)),
        ));
    }

    #[test]
    fn should_reload_fires_on_mtime_change() {
        let path = Some("/home/u/.config/felis/config.toml");
        assert!(should_reload(&st(path, Some(100)), &st(path, Some(101))));
    }

    #[test]
    fn should_reload_fires_when_nix_symlink_repoints_despite_frozen_mtime() {
        // `home-manager switch` repoints the symlink; every store file's
        // mtime is frozen at the epoch.
        let epoch = Some(1);
        assert!(should_reload(
            &st(Some("/nix/store/aaa-felis-config.toml"), epoch),
            &st(Some("/nix/store/bbb-felis-config.toml"), epoch),
        ));
    }

    #[test]
    fn should_reload_skips_when_fingerprint_unchanged() {
        let s = st(Some("/nix/store/aaa-felis-config.toml"), Some(1));
        assert!(!should_reload(&s, &s.clone()));
    }

    #[test]
    fn should_reload_skips_when_target_unresolved() {
        let had = st(Some("/nix/store/aaa-felis-config.toml"), Some(1));
        assert!(!should_reload(&had, &st(None, Some(1))));
        assert!(!should_reload(&had, &st(None, None)));
        assert!(!should_reload(&st(None, None), &st(None, None)));
    }
}

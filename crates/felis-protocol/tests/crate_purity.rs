//! Crate-purity guard ensuring `felis-protocol` remains free of async runtimes
//! and OS-specific dependencies (`docs/explanation/architecture/overview.md`
//! "Workspace: the crate-boundary decision record").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

/// Matched by exact crate name; banning a stem (`tokio*`) would catch
/// innocuous re-exports.
const BANNED: &[&str] = &[
    "tokio",
    "tokio-util",
    "tokio-stream",
    "async-std",
    "smol",
    "mio",
    "polling",
    "libc",
    "rustix",
    "nix",
    "windows",
    "windows-sys",
    "windows-targets",
    "core-foundation",
    "core-foundation-sys",
    "objc",
    "objc2",
];

#[test]
fn crate_purity_no_async_or_os_deps() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    let out = Command::new(&cargo)
        .args([
            "tree",
            "-p",
            "felis-protocol",
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--all-features",
            "--target",
            "all",
        ])
        .current_dir(manifest_dir)
        .output()
        .expect("`cargo tree` should be runnable from the test harness");

    assert!(
        out.status.success(),
        "cargo tree exited with {:?}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("cargo tree should produce utf-8");

    let mut violations = Vec::new();
    for line in stdout.lines() {
        // `--prefix none` lines are `name vX.Y.Z [...]`.
        let Some(name) = line.split_whitespace().next() else {
            continue;
        };
        if BANNED.contains(&name) {
            violations.push(name);
        }
    }
    violations.sort_unstable();
    violations.dedup();

    assert!(
        violations.is_empty(),
        "felis-protocol must stay tokio-free and OS-agnostic \
         (docs/explanation/architecture/overview.md \"Workspace\"); \
         banned production dep(s) reached: {violations:?}\n\nFull tree:\n{stdout}",
    );
}

// Included by every felis binary crate's build.rs via
// `include!("../build-common.rs")`; relative paths below resolve against
// the including crate's directory (`crates/<name>/`).

/// Human abbreviation width for a revision. Fixed here rather than
/// taken from `git rev-parse --short`, whose width follows the local
/// object store: two binaries built on two machines would otherwise
/// print hashes of different lengths and read as different builds.
const SHORT_REVISION: usize = 12;

/// Length of a full git revision, and the only length
/// `BuildIdentity::from_build_env` accepts.
const FULL_REVISION: usize = 40;

fn emit_build_id() {
    // Nix builds from a `.git`-less snapshot; nix/package.nix passes the
    // revision in through FELIS_GIT_HASH instead, in the same
    // `<rev>[-dirty]` shape `git` is asked for below.
    let stamp = std::env::var("FELIS_GIT_HASH")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(git_stamp)
        .unwrap_or_else(|| "unknown".to_owned());
    let (revision, dirty) = match stamp.strip_suffix("-dirty") {
        Some(revision) => (revision, true),
        None => (stamp.as_str(), false),
    };
    // Same rule `BuildIdentity::from_build_env` applies to the full
    // stamp: FELIS_GIT_HASH is an out-of-tree input, and abbreviating an
    // unvalidated one would leave `--version` naming a revision the typed
    // identity reports as unknown, so one binary would answer two ways
    // about its own build.
    let revision = if is_revision(revision) {
        revision
    } else {
        "unknown"
    };
    let short = &revision[..revision.len().min(SHORT_REVISION)];
    let suffix = if dirty { "-dirty" } else { "" };
    println!("cargo:rustc-env=FELIS_BUILD_STAMP={revision}{suffix}");
    println!("cargo:rustc-env=FELIS_BUILD_STAMP_SHORT={short}{suffix}");

    // `logs/HEAD` as well as `HEAD`: HEAD is a symref, so a commit onto
    // the same branch leaves it untouched. `index` catches a staged
    // change flipping the dirty bit; an *unstaged* edit does not
    // re-stamp, which is why the exact-identity guarantee belongs to the
    // Nix build and not to cargo (docs/reference/workspace.md).
    println!("cargo:rerun-if-env-changed=FELIS_GIT_HASH");
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    // Any rerun-if-changed replaces cargo's watch-the-package default, and
    // this file lives outside the package.
    println!("cargo:rerun-if-changed=../build-common.rs");
}

fn is_revision(s: &str) -> bool {
    s.len() == FULL_REVISION
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn git_stamp() -> Option<String> {
    let revision = git(&["rev-parse", "HEAD"])?;
    // `--untracked-files=no`: an untracked scratch file is not part of
    // what was compiled, and would mark every working tree dirty.
    let dirty =
        if git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty()) {
            "-dirty"
        } else {
            ""
        };
    Some(format!("{revision}{dirty}"))
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

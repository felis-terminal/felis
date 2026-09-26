//! Emits `felis_getpeereid` for the Apple/BSD targets. Cargo cannot gate a
//! dependency on a custom cfg, so `Cargo.toml`'s
//! `[target.'cfg(…)'.dependencies]` entry for `libc` repeats this list and
//! must stay in step.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(felis_getpeereid)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let getpeereid = matches!(
        target_os.as_str(),
        "macos" | "ios" | "tvos" | "watchos" | "freebsd" | "dragonfly" | "netbsd" | "openbsd"
    );
    if getpeereid {
        println!("cargo::rustc-cfg=felis_getpeereid");
    }
}

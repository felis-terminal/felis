//! `felis-grid` is daemon-side state and must not depend on font, shaping,
//! or rendering crates (`docs/explanation/principles.md` "3. The daemon
//! owns state, the client owns pixels";
//! `docs/explanation/architecture/overview.md` "Workspace: the
//! crate-boundary decision record").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

const BANNED: &[&str] = &[
    "swash",
    "zeno",
    "fontdb",
    "cosmic-text",
    "harfbuzz",
    "harfbuzz-sys",
    "harfbuzz_rs",
    "freetype",
    "freetype-rs",
    "freetype-sys",
    "ttf-parser",
    "ab_glyph",
    "ab_glyph_rasterizer",
    "wgpu",
    "wgpu-core",
    "wgpu-hal",
    "wgpu-types",
    "raw-window-handle",
    "winit",
    "bytemuck",
    "tiny-skia",
    "skia-safe",
    "lyon",
    "lyon_tessellation",
    "usvg",
    "resvg",
];

#[test]
fn felis_grid_has_no_render_or_shaping_deps() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let manifest_dir = env!("CARGO_MANIFEST_DIR");

    let out = Command::new(&cargo)
        .args([
            "tree",
            "-p",
            "felis-grid",
            "--edges",
            "normal,build",
            "--prefix",
            "none",
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
        "felis-grid stores raw state only — presentation belongs in \
         felis-shaping / felis-render-wgpu (the daemon owns state, the \
         client owns pixels). \
         Banned production dep(s) reached: {violations:?}\n\nFull tree:\n{stdout}",
    );
}

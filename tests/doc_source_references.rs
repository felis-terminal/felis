//! Pins source-file citations in the docs to files that exist in the workspace.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("workspace root is one parent up from felis-workspace-tests")
        .to_path_buf()
}

fn collect_markdown(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("readdir docs subdir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

/// Bare `crates/.../*.rs` citations in prose (optionally suffixed
/// `::test_name`), the style the Markdown-link guard does not inspect.
fn extract_crate_path_mentions(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let mut rest = line;
        while let Some(idx) = rest.find("crates/") {
            let from = &rest[idx..];
            let end = from
                .char_indices()
                .find(|(_, c)| {
                    c.is_whitespace() || matches!(*c, ',' | ')' | '`' | ']' | '"' | ';' | '<' | '>')
                })
                .map_or(from.len(), |(i, _)| i);
            let token = from[..end].trim_end_matches(['.', ',', ';', ':']);
            if let Some(rs_end) = token.find(".rs") {
                let path = &token[..rs_end + 3];
                out.insert(path.to_string());
            }
            rest = &from[end..];
        }
    }
    out
}

#[test]
fn every_bare_crate_path_in_docs_resolves() {
    let workspace = workspace_root();
    let docs_dir = workspace.join("docs");
    assert!(
        docs_dir.is_dir(),
        "docs/ not found at {} — test must run from a felis checkout",
        docs_dir.display(),
    );

    let mut violations: Vec<String> = Vec::new();
    let mut seen: BTreeSet<(PathBuf, String)> = BTreeSet::new();

    for source in collect_markdown(&docs_dir) {
        let body = fs::read_to_string(&source)
            .unwrap_or_else(|e| panic!("read {}: {}", source.display(), e));
        let source_rel = source.strip_prefix(&docs_dir).unwrap_or(&source);

        for token in extract_crate_path_mentions(&body) {
            if !seen.insert((source.clone(), token.clone())) {
                continue;
            }
            let resolved = workspace.join(&token);
            if !resolved.is_file() {
                violations.push(format!(
                    "{} mentions {} (resolved: {}) — file does not exist",
                    source_rel.display(),
                    token,
                    resolved.display(),
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "bare `crates/.../*.rs` citations in docs/ must resolve to \
         existing files. {} violation(s):\n  {}",
        violations.len(),
        violations.join("\n  "),
    );
}

#[cfg(test)]
mod parsers {
    use super::*;

    #[test]
    fn extract_crate_paths_picks_up_prose_and_inline_code_forms() {
        let text = "see crates/felis-grid/tests/snapshot_csi.rs and \
                    `crates/felis-vt/src/lib.rs` and \
                    crates/felis-daemon/tests/doc_source_references.rs.\n\
                    Trailing punctuation: crates/felis-grid/tests/proptest_grid.rs:: \
                    or crates/felis-vt/tests/proptest_parser.rs, \
                    or in parens: (crates/felis-protocol/src/lib.rs).";
        let got = extract_crate_path_mentions(text);
        let want: BTreeSet<String> = [
            "crates/felis-grid/tests/snapshot_csi.rs",
            "crates/felis-vt/src/lib.rs",
            "crates/felis-daemon/tests/doc_source_references.rs",
            "crates/felis-grid/tests/proptest_grid.rs",
            "crates/felis-vt/tests/proptest_parser.rs",
            "crates/felis-protocol/src/lib.rs",
        ]
        .iter()
        .map(|&s| s.to_string())
        .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn extract_crate_paths_strips_test_name_suffix_after_dot_rs() {
        let text = "see crates/felis-grid/tests/proptest_grid.rs::sized_runs_round_trip";
        let got = extract_crate_path_mentions(text);
        assert_eq!(got.len(), 1);
        assert!(got.contains("crates/felis-grid/tests/proptest_grid.rs"));
    }

    #[test]
    fn extract_crate_paths_skips_non_rs_mentions() {
        let text = "see crates/felis-grid/Cargo.toml and crates/felis-grid/";
        let got = extract_crate_path_mentions(text);
        assert!(got.is_empty());
    }
}

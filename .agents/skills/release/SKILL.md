---
name: release
description:
  Cut a felis release. Prepare the app and library versions in a release PR, tag, watch the Forgejo gate and the GitHub
  mirror publish the archives and the crates.io packages, and the one-time steps (first crates.io publish, tag
  protection, the crates-io environment). Use when preparing, tagging, or recovering a release.
---

# Cutting a release

What the gate checks and what each workflow publishes is `docs/reference/workspace.md` "Release gate"; the version rules
are its "Versioning". This skill is the order of operations.

## 1. Release PR

1. Bump `[workspace.package] version` in the root `Cargo.toml` (the app version: felis-cli, felis-client, felis-daemon)
   and add the `## [X.Y.Z] - YYYY-MM-DD` section to `CHANGELOG.md`.
2. `just release-prepare` (`release-plz update`): bumps every library whose package differs from its newest crates.io
   version, raises the requirements on it, and patch-bumps its dependents. Commit the manifests and `Cargo.lock`.
3. Review what the tools do not decide, and edit `[package] version`, the matching `[workspace.dependencies]` entry, and
   `cargo update --workspace` by hand where needed:
   - a dependent that re-exports or exposes types of a library that took a minor takes a minor too;
   - a change to a `cfg(windows)` or `cfg(target_os = "macos")` public item: the semver check runs on Linux only;
   - a break in felis-daemon's library API, or a minor of a library it depends on, needs an app minor.
4. `python3 tools/release/crates.py check` on the PR branch (needs crates.io) passes before merge.

## 2. Tag and watch

- `git tag -a vX.Y.Z <merge commit>` (`-rc.N` for a candidate) and push the tag; `just release-check vX.Y.Z` first.
- Forgejo `release.yml`: `verify-tag` (identity, then the crate check) gates everything; the release page publishes
  last.
- GitHub `release-mirror.yml`: `mirror` copies the release, then `crates` publishes the crates whose version is not on
  crates.io (final tags only) and `homebrew` bumps the tap. A `crates` failure is rerun as is: the plan shrinks to what
  is still unpublished.

## 3. One-time setup

- Protect `v*` tags from update and deletion on Forgejo (protected tags) and on GitHub (tag ruleset).
- On GitHub, create the `crates-io` environment.
- First crates.io publish (v0.3.0), after the Forgejo release is published, at the tag, with no `cargo login` and no
  credentials file:
  1. `cargo publish --workspace --dry-run` (verification builds, no credential present);
  2. `CARGO_REGISTRY_TOKEN=<publish-new token> cargo publish --workspace --no-verify`; after a partial failure, rerun
     both steps with `-p <crate>` for each crate `python3 tools/release/crates.py publish-plan` names as never
     published;
  3. on crates.io, add a trusted publisher to each crate: repository `felis-terminal/felis`, workflow
     `release-mirror.yml`, environment `crates-io`;
  4. revoke the token and rerun the `crates` job; it finds nothing to publish.

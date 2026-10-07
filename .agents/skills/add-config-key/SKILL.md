---
name: add-config-key
description:
  Add or change a felis config key end-to-end — decide the right control surface first, edit the felis-client-core
  config structs, regenerate the JSON schema with `just schema`, and cascade the reference docs. Use whenever a change
  adds/renames/retypes a key in config.toml, adds a keybinding action, or moves a knob between surfaces (config vs CLI
  flag vs env var vs keybind). Covers the stale-schema CI failure, closed-enum wiring, the home-manager module, and how
  older builds read a newer config.
allowed-tools: Read Grep Edit Bash(just:*) Bash(cargo:*)
---

# Adding a config key

## 1. Is config.toml even the right surface?

felis has five control surfaces (daemon CLI, client CLI, config file, keybind, environment) and a placement criterion
for which knob goes where: `docs/explanation/architecture/control-surfaces.md` (the argument) and
`docs/reference/control-surfaces.md` (the name map). Check both before adding a key; the asymmetries (some knobs exist
on purpose on only one surface) are deliberate and documented there. Remember principle 3: config is read by the
**client**; a key that would change daemon-owned state is a design smell, not a config key.

If the "key" is really a new capability, run the `principle-check` skill first.

## 2. Code

- Structs live in `crates/felis-client-core/src/config.rs`. Read its module comment first: the schema is intentionally
  **additive**. New fields take `#[serde(default)]` so older files keep parsing; unknown keys are _not_ parse errors but
  are reported through `ConfigDocument::resolve`'s `ConfigDiagnostics` so a typo never goes silently dead. Follow that
  posture: a new key must never make an existing config file fail to load.
- Sections live on `EffectiveConfig` (defaults applied, the client's `[client.<id>]` overlay folded in). Validating a
  new key belongs in `config/validate.rs`, which _collects_ diagnostics: never log from a validator; the loader logs the
  whole set once, and `felis config check` prints it instead.
- A file-valued key resolves through `config::resolve_path` against the `config.toml`'s own directory
  (`EffectiveConfig::source_dir`), never the process's working directory; a missing target is a warning diagnostic, not
  an error.
- A key that takes a token from a closed set with a documented default (REQ-1103) needs three things so an unknown token
  degrades that one field instead of failing the document: `#[serde(deserialize_with = "deserialize_lenient_enum")]` on
  the field, an `enum_token` entry in `closed_enums` (`config/validate.rs`) naming its path and default so the loader
  warns, and a mention in the closed-set list of `docs/reference/config.md`.
- Keybinding actions: the action enum in `crates/felis-client-core/src/action.rs` and the keymap under
  `crates/felis-client-core/src/keymap*`. Action variants may carry typed data (`SendString { text }`), never
  expressions or callbacks (principle 1).
- Add/extend the parse-and-default tests beside the existing ones in `config.rs`.

## 3. Regenerate the schema (CI fails without this)

```sh
just schema     # UPDATE_SCHEMA=1 cargo test -p felis-client-core --features schema config_schema
```

Commit the regenerated `crates/felis-client-core/felis-config.schema.json` together with the struct change. `just test`
/ CI re-derive it under `--all-features` and fail if the committed copy is stale. Never hand-edit it.

## 4. Doc cascade

- `docs/reference/config.md`: the key, default, range, and behavior; a closed-enum key also joins the list of keys that
  "take a token from a closed set" (and its count).
- `docs/reference/control-surfaces.md`: add the row to the name map if the knob exists on more than one surface.
- `docs/reference/keybindings.md`: if a new action or default chord.
- `skills/felis/SKILL.md` and Forgejo issues (if the key appears there); on a rename, also `nix/hm-module.nix` /
  `nix/stylix.nix` and the repo-root `README.md`.
- If the _decision_ is non-obvious (why this surface, what was rejected), record it inline in the owning explanation doc
  (see the `doc-cascade` skill for the full sweep procedure).

## 5. Home-manager module

`nix/hm-module.nix` passes `settings` through as freeform TOML, so a new _key_ needs no module change, but its option
docstring enumerates the top-level sections (`[font]`, `[theme]`, …); adding a new **section** means updating that list.

## Gotchas

- A float-typed key takes either TOML spelling: `size_px = 14` and `size_px = 14.0` both deserialize into `f32`.
- An older build warns on a key it does not know and ignores it, and degrades an unknown closed-enum token to that
  field's default; a value whose _shape_ it does not accept (a table where it expects a string) fails the whole document
  and falls back to **all defaults**. When A/B-comparing against an older commit, strip new keys and reshaped values
  from the test config, or the comparison silently tests something other than what the config says.
- The user's real config is often a read-only home-manager symlink; for experiments name a copy with
  `felis --config <absolute path>`, which touches no environment. A scratch `$HOME` / `$XDG_CONFIG_HOME` leaks into the
  auto-spawned daemon; see `isolated-daemon`.
- The client watches and hot-reloads the config file (`crates/felis-client-core/src/config_watcher.rs`). A new key that
  must not hot-apply needs an explicit decision, not an accident.

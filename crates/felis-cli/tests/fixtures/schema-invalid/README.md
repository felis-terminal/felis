Fixtures the published schema bundle must reject.

`out-*.json` are `{"against": "<$defs entry of felis-cli-v1>", "instance": {…}, "repaired": {…}}`: an output object is
validated against the class it claims, because payload objects stay open and a wrong object can otherwise satisfy a
laxer sibling in the union. `repaired` is the same object with only the targeted defect corrected and must validate, so
the fixture cannot pass for an unrelated reason.

`req-*.json` are whole `felis bridge` stdin lines, validated against `felis-bridge-v1#/$defs/request`; the same lines
are fed to a live bridge, which must answer `malformed_request`.

An `above-u64` / `above-i64` fixture carries the closest value the validator can still reject, which is one `f64` step
past the bound rather than the bound plus one: a 64-bit maximum is not representable as `f64`, and the validator
compares numbers as `f64`. `schema_fixtures.rs` pins that gap itself.

`tests/schema_fixtures.rs` and `tests/cli_bridge.rs` read them.

# Fuzz seed corpus

Bug-trail seeds for the cargo-fuzz targets. Each file is a raw byte input that previously surfaced a real bug, a known
edge case, or a spec-corner the fuzzer should explore early. Priming libFuzzer with these speeds up coverage discovery
and pins regression cases that might otherwise drift out of the working corpus over time (`cargo fuzz cmin` minimizes by
coverage, not provenance).

## Layout

```
fuzz/seeds/
├── vt_parser/         — bytes for `felis_vt::Parser::advance` (raw VT stream)
├── grid_dispatch/     — same shape as vt_parser, but exercises Grid handlers
├── kitty_graphics/    — APC G… body (the bytes between `\x1b_G` and `\x1b\\`)
├── kitty_text_sizing/ — OSC 66 metadata portion (the bytes before the `;`)
├── ipc_frame/         — `[ceiling:LE u32][frame_bytes…]` per the fuzz target's
│                        input convention; frame fmt is `[len:LE u32]
│                        [kind:LE u16][body]`
├── ipc_body/          — protobuf framed-body bytes for the body codec +
│                        the wire→domain conversion layer (including
│                        retired enum values, which must be rejected)
├── row_codec/         — `packed_cells` payloads per
│                        docs/reference/row-codec.md: the three golden
│                        vectors that spec publishes, plus one seed per
│                        rejection class (over-limit prefix, run/grapheme
│                        mismatch, trailing bytes, unknown version, …)
└── chord_parser/      — UTF-8 chord strings for
                         `felis_client_core::keymap::Chord::from_str`
                         (the `[keymap]` chord grammar of
                         docs/reference/config.md; non-UTF-8 bytes are
                         dropped by the target before parsing)
```

`fuzz/corpus/` is gitignored (that is where libFuzzer writes new inputs during a run; mixing those with the curated
seeds would defeat the gitignore). Seeds live in `fuzz/seeds/` and are copied into `corpus/` before a run.

## Workflow

```
# Prime the corpus with the curated seeds, then run.
fuzz/seed-corpus.sh
cargo fuzz run vt_parser
```

`seed-corpus.sh` copies `fuzz/seeds/<target>/*` into `fuzz/corpus/<target>/` for every target. Re-running it later is
safe (cargo-fuzz dedupes by content).

## Adding a seed

1. Reproduce the bug or pinpoint the edge case to a minimal byte sequence.
2. Write it to `fuzz/seeds/<target>/<descriptive-kebab-name>.bin` with `printf` (escape every non-ASCII byte explicitly
   so the file is unambiguous even when reviewed via `cat -v`):

   ```sh
   printf 'hello\x1b[1;1H\x1b[1P' > fuzz/seeds/grid_dispatch/dch-sl-repro.bin
   ```

3. Re-run the seed-corpus script and confirm `cargo fuzz run <target>` accepts the new input cleanly.

The naming convention is `<scope>-<short-tag>.bin` where the tag gives a human a one-glance read on what the seed
exercises:

- `dch-sl-repro.bin` — DCH (delete-character) bug `sl` triggered
- `su-clean-region-a4-finding.bin` — SU clean-region path the A4 proptest discovered
- `alt-screen-toggle.bin` — `?1049h` / `?1049l` pair (per-screen Kitty placement contexts)
- `osc-66-sized-cell.bin` — OSC 66 text-sizing happy-path body
- `body-over-ceiling.bin` — frame whose body exceeds the ceiling (ipc_frame must error rather than allocate)

## Cross-references

- `docs/reference/testing.md` — fuzz targets + acceptance criteria
- `docs/explanation/security-model.md` "Parser robustness" — 24-hour run target
- `.forgejo/workflows/fuzz.yml` — PR smoke + nightly long-run

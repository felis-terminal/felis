//! Pins the minor ledger in `docs/reference/ipc.md` to felis-protocol's executable metadata.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use felis_protocol::PROTOCOL_MINOR;
use felis_protocol::minor::{ADDITIONS, ROW_CODEC_SINCE};

fn ipc_reference() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/reference/ipc.md");
    std::fs::read_to_string(path).expect("docs/reference/ipc.md beside the workspace")
}

/// The `Addition` cell of each ledger row in `docs/reference/ipc.md`,
/// indexed by minor.
fn ledger_rows() -> Vec<String> {
    let doc = ipc_reference();
    doc.split("### The minor ledger")
        .nth(1)
        .expect("ipc.md has a \"The minor ledger\" section")
        .lines()
        .skip_while(|line| !line.starts_with("| Minor |"))
        .skip(2)
        .take_while(|line| line.starts_with('|'))
        .map(|row| {
            row.trim_start_matches('|')
                .split('|')
                .nth(1)
                .expect("a ledger row has an Addition cell")
                .to_owned()
        })
        .collect()
}

/// The reference doc's ledger table has one row per minor from 0 to
/// the current one, in order: a bump with no row is the drift this
/// test exists to catch.
#[test]
fn the_minor_ledger_has_a_row_per_minor() {
    let minors: Vec<u16> = (0..=PROTOCOL_MINOR).collect();

    let doc = ipc_reference();
    let table = doc
        .split("### The minor ledger")
        .nth(1)
        .expect("ipc.md has a \"The minor ledger\" section");
    let documented: Vec<u16> = table
        .lines()
        .skip_while(|l| !l.starts_with("| Minor |"))
        .skip(2)
        .take_while(|l| l.starts_with('|'))
        .map(|row| {
            row.trim_start_matches('|')
                .split('|')
                .next()
                .expect("a table row has a first cell")
                .trim()
                .parse()
                .expect("the ledger's first column is the minor")
        })
        .collect();
    assert_eq!(
        documented, minors,
        "the minor ledger table in docs/reference/ipc.md lacks a row per minor"
    );
}

/// The prose ledger and the executable metadata must cover the same
/// additions: the table is as long as the highest minor any arm,
/// field, enum value, or row codec claims, and every post-baseline
/// one is named in its own row. Forgetting either half is what lets
/// an addition reach the wire with no authorization behind it.
#[test]
fn the_ledger_covers_every_executable_addition() {
    let rows = ledger_rows();
    let named = ADDITIONS;

    let highest = named
        .iter()
        .map(|(_, minor)| *minor)
        .chain(ROW_CODEC_SINCE.iter().map(|(_, minor)| *minor))
        .max()
        .expect("the metadata names at least the base schema");
    assert_eq!(
        rows.len(),
        usize::from(highest) + 1,
        "the ledger table has {} rows for additions reaching minor {highest}",
        rows.len()
    );

    for (name, minor) in named {
        let minor = *minor;
        if minor == 0 {
            continue;
        }
        let row = &rows[usize::from(minor)];
        assert!(
            ledger_identifiers(row).contains(name),
            "minor {minor} authorizes `{name}`, but the ledger row for minor {minor} \
             does not name it: add it to the table in docs/reference/ipc.md"
        );
    }
    for (codec, minor) in ROW_CODEC_SINCE {
        if *minor == 0 {
            continue;
        }
        assert!(
            rows[usize::from(*minor)].contains(&format!("row-codec version {codec}")),
            "minor {minor} defines row-codec version {codec}, but its ledger row does not \
             name it"
        );
    }

    for (minor, row) in rows.iter().enumerate() {
        let minor = u16::try_from(minor).expect("the ledger is shorter than 65536 rows");
        let spans = ledger_identifiers(row);
        assert!(
            minor == 0 || !spans.is_empty(),
            "the ledger row for minor {minor} names no addition by identifier: \
             spell it as `felis.proto` does, so the metadata check can read it"
        );
        for span in spans {
            let found = named.iter().find(|(name, _)| *name == span);
            let Some((name, claimed)) = found else {
                panic!(
                    "the ledger row for minor {minor} names `{span}`, which no arm, \
                     field or enum value declares: give it metadata in \
                     crates/felis-protocol or drop the identifier from the row"
                );
            };
            assert_eq!(
                *claimed, minor,
                "the ledger row for minor {minor} names `{span}`, but `{name}` \
                 is authorized from minor {claimed}"
            );
        }
    }
}

/// Every ledger identifier in `row`: an arm as `Family::Variant`, a
/// closed-enum value as `Enum::VALUE`, a field as `Message.field`.
/// A backticked span that is none of those is prose the row is free
/// to carry (a bare type name, a variant of an inner enum, a command
/// line), which the reverse check must not read as an addition.
fn ledger_identifiers(row: &str) -> Vec<&str> {
    row.split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| {
            let qualified = span
                .split_once("::")
                .or_else(|| span.split_once('.'))
                .is_some_and(|(owner, member)| {
                    owner.starts_with(char::is_uppercase) && !member.is_empty()
                });
            qualified
                && span
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':' || c == '.')
        })
        .collect()
}

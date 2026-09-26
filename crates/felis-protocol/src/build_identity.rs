//! [`BuildIdentity`]: which build a felis process is, as one typed
//! value the three binaries share.
//!
//! Every process (front door, GUI client, daemon) renders and reads
//! the one canonical line (`docs/reference/workspace.md` "Versioning").

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The revision of a build whose git state was not available: a
/// tarball, or a Nix path built without `gitHash`.
pub const UNKNOWN_REVISION: &str = "unknown";

/// Length a full git revision has, and the only length the canonical
/// form accepts. Abbreviations float with the local object store, so
/// letting one on the wire would make two identities of one build.
const FULL_REVISION: usize = 40;

/// Digits [`BuildIdentity::human`] abbreviates a revision to.
const SHORT_REVISION: usize = 12;

/// Semver, exact source revision, and dirty status.
///
/// The canonical round-tripping form is `<version> (<revision>[-dirty])`.
/// [`BuildIdentity::human`] provides an abbreviated non-round-tripping form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildIdentity {
    /// Cargo package semver, e.g. `0.1.0`.
    pub version: String,
    /// Full lowercase-hex git revision, or [`UNKNOWN_REVISION`].
    pub revision: String,
    /// The working tree had uncommitted tracked changes when this was
    /// built, so `revision` names its parent, not its source.
    pub dirty: bool,
}

impl BuildIdentity {
    /// Builds the identity a binary stamps into itself from its
    /// `build.rs` constants: `stamp` is `<revision>[-dirty]`.
    ///
    /// Total: a build script has nobody to report a parse failure to,
    /// so a stamp that is not a revision becomes [`UNKNOWN_REVISION`].
    #[must_use]
    pub fn from_build_env(version: &str, stamp: &str) -> Self {
        let (revision, dirty) = match stamp.strip_suffix("-dirty") {
            Some(revision) => (revision, true),
            None => (stamp, false),
        };
        Self {
            version: version.to_owned(),
            revision: if is_revision(revision) {
                revision.to_owned()
            } else {
                UNKNOWN_REVISION.to_owned()
            },
            dirty,
        }
    }

    /// `<version> (<revision-12>[-dirty])`: the rendering for a person
    /// reading a terminal, never a parse target.
    #[must_use]
    pub fn human(&self) -> String {
        // Truncating by character, not by byte: an identity off the wire
        // is deliberately unvalidated (`convert::conn`), so a revision
        // may be any UTF-8 and a byte index at 12 would panic mid
        // character.
        let end = self
            .revision
            .char_indices()
            .nth(SHORT_REVISION)
            .map_or(self.revision.len(), |(i, _)| i);
        let revision = &self.revision[..end];
        format!(
            "{version} ({revision}{})",
            self.suffix(),
            version = self.version
        )
    }

    const fn suffix(&self) -> &'static str {
        if self.dirty { "-dirty" } else { "" }
    }
}

impl fmt::Display for BuildIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}{})", self.version, self.revision, self.suffix())
    }
}

/// Why a line is not a canonical identity.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a build identity (`<version> (<revision>[-dirty])`): {line}")]
pub struct BuildIdentityParseError {
    pub line: String,
}

impl FromStr for BuildIdentity {
    type Err = BuildIdentityParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let fail = || BuildIdentityParseError { line: s.to_owned() };
        let s = s.trim();
        let (version, rest) = s.split_once(" (").ok_or_else(fail)?;
        let stamp = rest.strip_suffix(')').ok_or_else(fail)?;
        let (revision, dirty) = match stamp.strip_suffix("-dirty") {
            Some(revision) => (revision, true),
            None => (stamp, false),
        };
        // A version with a space in it means the caller left the
        // binary name on the front of a `--version` line; taking it as
        // the version would make two builds' identities compare unequal
        // for the name alone.
        if version.is_empty() || version.contains(char::is_whitespace) || !is_revision(revision) {
            return Err(fail());
        }
        Ok(Self {
            version: version.to_owned(),
            revision: revision.to_owned(),
            dirty,
        })
    }
}

fn is_revision(s: &str) -> bool {
    s == UNKNOWN_REVISION
        || (s.len() == FULL_REVISION
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const REV: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn a_clean_build_renders_the_revision_alone() {
        let id = BuildIdentity::from_build_env("0.1.0", REV);
        assert!(!id.dirty);
        assert_eq!(id.to_string(), format!("0.1.0 ({REV})"));
        assert_eq!(id.human(), "0.1.0 (0123456789ab)");
    }

    #[test]
    fn a_dirty_build_renders_the_suffix_in_both_forms() {
        let id = BuildIdentity::from_build_env("0.1.0", &format!("{REV}-dirty"));
        assert!(id.dirty);
        assert_eq!(id.revision, REV);
        assert_eq!(id.to_string(), format!("0.1.0 ({REV}-dirty)"));
        assert_eq!(id.human(), "0.1.0 (0123456789ab-dirty)");
    }

    #[test]
    fn a_stamp_that_is_not_a_revision_is_unknown_rather_than_a_panic() {
        let id = BuildIdentity::from_build_env("0.1.0", "b27da75");
        assert_eq!(id.revision, UNKNOWN_REVISION);
        assert_eq!(id.human(), "0.1.0 (unknown)");
    }

    /// A revision that never went through `is_revision` (the wire
    /// conversion is infallible by design) still renders rather than
    /// panicking on a byte index inside a multi-byte character.
    #[test]
    fn an_unvalidated_revision_renders_instead_of_panicking() {
        let id = BuildIdentity {
            version: "0.1.0".to_owned(),
            revision: "0123456789aé".to_owned(),
            dirty: false,
        };
        assert_eq!(id.human(), "0.1.0 (0123456789aé)");

        let id = BuildIdentity {
            version: "0.1.0".to_owned(),
            revision: "ééééééééééééé".to_owned(),
            dirty: true,
        };
        assert_eq!(id.human(), "0.1.0 (éééééééééééé-dirty)");
    }

    /// The abbreviated rendering is explicitly not a parse target: a
    /// consumer that fed it back would silently accept two different
    /// builds as one.
    #[test]
    fn the_abbreviated_rendering_does_not_parse() {
        let id = BuildIdentity::from_build_env("0.1.0", REV);
        assert!(id.human().parse::<BuildIdentity>().is_err());
    }

    #[test]
    fn lines_outside_the_canonical_form_are_refused() {
        for line in [
            "0.1.0",
            "felis 0.1.0 (0123456789abcdef0123456789abcdef01234567)",
            "0.1.0 (0123456789ABCDEF0123456789ABCDEF01234567)",
            "0.1.0 ()",
            " (0123456789abcdef0123456789abcdef01234567)",
        ] {
            assert!(
                line.parse::<BuildIdentity>().is_err(),
                "accepted `{line}` as canonical"
            );
        }
    }

    proptest! {
        /// Oracle built by hand rather than by `Display`, so the two
        /// halves cannot agree on a shape neither surface asked for.
        #[test]
        fn every_identity_round_trips_through_its_canonical_line(
            version in "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}",
            revision in prop_oneof!["[0-9a-f]{40}", Just(UNKNOWN_REVISION.to_owned())],
            dirty in any::<bool>(),
        ) {
            let line = format!(
                "{version} ({revision}{})",
                if dirty { "-dirty" } else { "" },
            );
            let parsed: BuildIdentity = line.parse().unwrap();
            prop_assert_eq!(&parsed.version, &version);
            prop_assert_eq!(&parsed.revision, &revision);
            prop_assert_eq!(parsed.dirty, dirty);
            prop_assert_eq!(parsed.to_string(), line);
        }
    }
}

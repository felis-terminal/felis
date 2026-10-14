//! `felis fonts list`: the font family names felis accepts, read from
//! `felis-client --font-families-probe`, which links the shaping stack
//! this binary must not (docs/reference/cli.md "Fonts").

use std::ffi::{OsStr, OsString};

use clap::Subcommand;
use felis_client_core::font_families::{
    FAMILIES_PROBE_VERSION, FONT_FAMILIES_PROBE_FLAG, FamiliesOutcome, FamiliesProbe, Family, Style,
};

use crate::cli_output::{
    ErrorKind, FontFamiliesResult, FontFamilyObject, FontStyleObject, Format, PointFormat,
    Reporter, stdoutln,
};
use crate::frontend_probe::{self, Cause};

#[derive(Debug, Subcommand)]
pub(crate) enum FontsOp {
    /// List the font family names the `family` config keys accept.
    ///
    /// One family per line, sorted ignoring case, with whether its
    /// printable ASCII is fixed-pitch and the styles installed. A name
    /// outside this list makes a window fall back to the default
    /// monospace face, unless a fontconfig alias (`monospace`, a
    /// `fonts.conf` `<alias>`) maps it.
    List {
        #[command(flatten)]
        output: PointFormat,
    },
}

impl FontsOp {
    pub(crate) const fn format(&self) -> Format {
        match self {
            Self::List { output } => output.format,
        }
    }
}

pub(crate) fn run(op: FontsOp, client_program: impl FnOnce() -> OsString) -> i32 {
    match op {
        FontsOp::List { output } => {
            let out = Reporter::point(output.format);
            match list_families(&client_program()) {
                Ok(families) => {
                    if out.machine() {
                        out.result(&FontFamiliesResult {
                            families: families.into_iter().map(family_object).collect(),
                        });
                    } else {
                        for line in table(&families) {
                            stdoutln!("{line}");
                        }
                    }
                    0
                }
                Err((kind, message)) => out.fail(kind, message),
            }
        }
    }
}

fn family_object(family: Family) -> FontFamilyObject {
    FontFamilyObject {
        name: family.name,
        monospace: family.monospace,
        styles: family
            .styles
            .into_iter()
            .map(|style| FontStyleObject {
                weight: style.weight,
                italic: style.italic,
            })
            .collect(),
    }
}

fn table(families: &[Family]) -> Vec<String> {
    const HEADER: &str = "FAMILY";
    let width = |name: &str| -> usize {
        name.chars()
            .map(|c| usize::from(felis_grid::char_cell_width(c)))
            .sum()
    };
    let column = families
        .iter()
        .map(|family| width(&family.name))
        .chain([HEADER.len()])
        .max()
        .unwrap_or_default();
    let row = |name: &str, mono: &str, styles: &str| {
        let pad = " ".repeat(column - width(name));
        format!("{name}{pad}  {mono:<4}  {styles}")
    };
    let mut lines = vec![row(HEADER, "MONO", "STYLES")];
    lines.extend(families.iter().map(|family| {
        row(
            &family.name,
            if family.monospace { "yes" } else { "no" },
            &style_labels(&family.styles),
        )
    }));
    lines
}

/// Weights between the named ones take the nearest name, so two faces
/// may share a label; `--format json` carries the exact weights.
fn style_labels(styles: &[Style]) -> String {
    const NAMES: [(u16, &str); 9] = [
        (100, "Thin"),
        (200, "ExtraLight"),
        (300, "Light"),
        (400, "Regular"),
        (500, "Medium"),
        (600, "SemiBold"),
        (700, "Bold"),
        (800, "ExtraBold"),
        (900, "Black"),
    ];
    let mut labels: Vec<String> = Vec::new();
    for style in styles {
        let weight = NAMES
            .iter()
            .min_by_key(|(weight, _)| weight.abs_diff(style.weight))
            .map_or("Regular", |&(_, name)| name);
        let label = match (weight, style.italic) {
            ("Regular", true) => "Italic".to_owned(),
            (weight, true) => format!("{weight} Italic"),
            (weight, false) => weight.to_owned(),
        };
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    labels.join(", ")
}

/// `unsupported` when the installed frontend cannot answer at all,
/// `internal` when it answered and the listing failed.
fn list_families(program: &OsStr) -> Result<Vec<Family>, (ErrorKind, String)> {
    let text = frontend_probe::run(program, FONT_FAMILIES_PROBE_FLAG.as_ref()).map_err(
        |failure| match failure.cause {
            Cause::Absent => (
                ErrorKind::Unsupported,
                format!(
                    "{}; listing fonts needs the felis GUI frontend, which links the font stack \
                     a window draws with",
                    failure.message
                ),
            ),
            Cause::UnknownFlag => (
                ErrorKind::Unsupported,
                "this felis-client predates `felis fonts list`; install the felis-client from \
                 the same release as this felis"
                    .to_owned(),
            ),
            Cause::Failed => (ErrorKind::Internal, failure.message),
        },
    )?;
    let probe: FamiliesProbe = serde_json::from_str(text.trim()).map_err(|err| {
        (
            ErrorKind::Internal,
            format!("the frontend's font listing did not parse: {err}"),
        )
    })?;
    if probe.v != FAMILIES_PROBE_VERSION {
        return Err((
            ErrorKind::Unsupported,
            format!(
                "the frontend speaks font-listing version {} and this build reads \
                 {FAMILIES_PROBE_VERSION}; the two binaries are from different releases",
                probe.v
            ),
        ));
    }
    match probe.outcome {
        FamiliesOutcome::Listed { families } => Ok(families),
        FamiliesOutcome::Failed { error } => Err((
            ErrorKind::Internal,
            format!("the frontend could not list fonts: {error}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::test_support::{fake_frontend, private_dir};

    #[cfg(unix)]
    fn listed_by(script: &str) -> Result<Vec<Family>, (ErrorKind, String)> {
        let dir = private_dir();
        list_families(&fake_frontend(dir.path(), script))
    }

    #[cfg(unix)]
    #[test]
    fn the_frontends_listing_is_passed_through_in_its_order() {
        let families = listed_by(
            r#"[ "$1" = --font-families-probe ] || exit 2
echo '{"v":2,"families":[{"name":"DejaVu Sans Mono","monospace":true,"styles":[]},{"name":"Noto Color Emoji","monospace":false,"styles":[]}]}'"#,
        )
        .unwrap();
        let names: Vec<&str> = families.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["DejaVu Sans Mono", "Noto Color Emoji"]);
    }

    fn family(name: &str, monospace: bool, styles: &[(u16, bool)]) -> Family {
        Family {
            name: name.to_owned(),
            monospace,
            styles: styles
                .iter()
                .map(|&(weight, italic)| Style { weight, italic })
                .collect(),
        }
    }

    #[test]
    fn the_table_aligns_names_by_their_cell_width() {
        let lines = table(&[
            family(
                "Menlo",
                true,
                &[(400, false), (700, false), (400, true), (700, true)],
            ),
            family("ヒラギノ角ゴシック", false, &[(300, false)]),
        ]);
        assert_eq!(
            lines,
            [
                "FAMILY              MONO  STYLES",
                "Menlo               yes   Regular, Bold, Italic, Bold Italic",
                "ヒラギノ角ゴシック  no    Light",
            ]
        );
    }

    #[test]
    fn weights_between_named_ones_share_the_nearest_label() {
        let styles = [(380, false), (400, false), (950, true)]
            .map(|(weight, italic)| Style { weight, italic });
        assert_eq!(style_labels(&styles), "Regular, Black Italic");
    }

    /// clap's refusal of the unknown flag is what a frontend from before
    /// this verb answers.
    #[cfg(unix)]
    #[test]
    fn a_frontend_that_predates_the_flag_is_unsupported() {
        let (kind, message) =
            listed_by(r#"echo "error: unexpected argument '$1' found" >&2; exit 2"#).unwrap_err();
        assert_eq!(kind, ErrorKind::Unsupported);
        assert!(message.contains("predates"), "{message}");
    }

    #[test]
    fn an_absent_frontend_is_unsupported() {
        let (kind, message) = list_families("felis-client-that-is-not-there".as_ref()).unwrap_err();
        assert_eq!(kind, ErrorKind::Unsupported);
        assert!(message.contains("could not be run"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_enumeration_keeps_the_frontends_reason() {
        let (kind, message) =
            listed_by(r#"echo '{"v":2,"error":"fontconfig: load libfontconfig.so.1"}'"#)
                .unwrap_err();
        assert_eq!(kind, ErrorKind::Internal);
        assert!(message.contains("libfontconfig"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn a_listing_from_a_different_epoch_is_refused() {
        let (kind, message) = listed_by(r#"echo '{"v":3,"families":[]}'"#).unwrap_err();
        assert_eq!(kind, ErrorKind::Unsupported);
        assert!(message.contains("different releases"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn a_crashing_frontend_is_internal() {
        let (kind, _) = listed_by("exit 101").unwrap_err();
        assert_eq!(kind, ErrorKind::Internal);
    }
}

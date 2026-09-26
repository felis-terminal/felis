//! Per-operation payload limits (REQ-105a).
//!
//! Validates frame payloads against operational ceilings before wire encoding
//! and after wire decoding. Framing ceilings are in [`crate::frame::DEFAULT_MAX_BODY`].

use crate::convert::WireError;
use crate::messages::{
    AttachTarget, InputMsg, Key, MAX_ENV_BASE_BYTES, MAX_ENV_BASE_ENTRIES, MAX_SESSION_TAGS,
    MAX_TAG_BYTES, OpsToDaemonMsg, PushMsg, RetargetCarrier, RetargetLanding, RetargetTarget,
    SearchToDaemonMsg, SessionToDaemonMsg, SpawnArgs, SwitchTarget,
};
use crate::session_prefix::validate_session_id_prefix;

/// Maximum bytes one [`InputMsg::Paste`] may carry (equal to
/// [`crate::limits::MAX_PASTE_BYTES`]).
///
/// Senders preflight against the exact admission limit to prevent
/// unexpected wire refusals.
pub const MAX_PASTE_BYTES: usize = crate::limits::MAX_PASTE_BYTES;

/// Maximum bytes one [`InputMsg::KeyBytes`] may carry.
///
/// Sized to the full PTY input budget because raw input streams (e.g. from
/// `felis sessions send --raw`) can carry escape sequences or replayed streams.
pub const MAX_RAW_INPUT_BYTES: usize = crate::limits::PTY_INPUT_BUDGET;

/// Maximum bytes a [`Key::Character`] may carry (equal to
/// [`crate::limits::MAX_KEY_CHARACTER_BYTES`]).
pub const MAX_KEY_CHARACTER_BYTES: usize = crate::limits::MAX_KEY_CHARACTER_BYTES;

/// Maximum bytes a [`crate::messages::KeyEvent::text`] may carry (equal
/// to [`crate::limits::MAX_KEY_TEXT_BYTES`]).
pub const MAX_KEY_TEXT_BYTES: usize = crate::limits::MAX_KEY_TEXT_BYTES;

/// Bytes a [`SearchToDaemonMsg::Query`] pattern may carry. This bounds the
/// request; the `regex` crate applies its own compiled-size limit to
/// the automaton the pattern builds.
pub const MAX_SEARCH_PATTERN_BYTES: usize = 4 * 1024;

/// Entries a [`SpawnArgs::args`] vector may carry, mirroring
/// [`MAX_ENV_BASE_ENTRIES`].
pub const MAX_SPAWN_ARGV_ENTRIES: usize = 4096;

/// Total bytes a [`SpawnArgs::args`] vector may carry. Linux counts
/// argv and the environment together against a 2 MiB `ARG_MAX`, so
/// this and [`MAX_ENV_BASE_BYTES`] split that budget.
pub const MAX_SPAWN_ARGV_BYTES: usize = 1 << 20;

/// Bytes a [`SpawnArgs::command`] or [`SpawnArgs::cwd`] may carry:
/// `PATH_MAX` on the platforms felis runs on.
pub const MAX_SPAWN_PATH_BYTES: usize = 4 * 1024;

/// Total bytes the strings of one [`RetargetTarget`] carrier may carry:
/// an SSH destination plus its `--ssh-arg` tokens.
pub const MAX_RETARGET_DESCRIPTOR_BYTES: usize = 64 * 1024;

/// Maximum bytes for one request line of the CLI stdio bridge.
///
/// Sized to 8x [`MAX_PASTE_BYTES`] to allow for sixfold JSON unicode escaping
/// of control bytes plus request envelope overhead.
pub const MAX_BRIDGE_LINE_BYTES: usize = 8 * MAX_PASTE_BYTES;

// The sixfold expansion is what fixes the multiplier, so the two must
// not drift apart: a later paste cap raises this one with it.
const _: () = assert!(MAX_BRIDGE_LINE_BYTES > 6 * MAX_PASTE_BYTES);

/// Sender-side ceiling for [`crate::messages::RegionToClientMsg::Reply`] bytes.
///
/// Sized to half the framing backstop. Senders trim replies to keep the youngest
/// bytes without exceeding framing limits or breaking backward compatibility.
pub const MAX_REGION_REPLY_BYTES: usize = 32 * 1024 * 1024;

/// Maximum decoded bytes one image may occupy (REQ-1008).
///
/// Evaluated at `u64` wire width before narrowing to validate image buffer
/// allocation claims on both daemon and client.
pub const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

/// Decoded bytes one session's images may retain in total (REQ-1008),
/// across every image and every animation frame. The daemon evicts to
/// stay under it; a receiver mirroring that store refuses a claim that
/// would carry it past.
pub const MAX_SESSION_IMAGE_BYTES: usize = 256 * 1024 * 1024;

/// Maximum frames one image may hold, root frame included (REQ-1008).
///
/// Bounds slot allocations across individually sized animation frames
/// where byte-size limits alone would admit millions of 1x1 frames.
pub const MAX_IMAGE_FRAMES: usize = 4096;

// Invariant: frame body limits must stay below the framing backstop.
// Stdio bridge lines use pipes rather than frame bodies; image limits bound
// decoded pixel claims rather than single frame bodies.
const _: () = {
    let backstop = crate::frame::DEFAULT_MAX_BODY as usize;
    assert!(MAX_RAW_INPUT_BYTES < backstop);
    assert!(MAX_PASTE_BYTES < backstop);
    assert!(MAX_SEARCH_PATTERN_BYTES < backstop);
    assert!(MAX_SPAWN_ARGV_BYTES < backstop);
    assert!(MAX_SPAWN_PATH_BYTES < backstop);
    assert!(MAX_RETARGET_DESCRIPTOR_BYTES < backstop);
    assert!(MAX_REGION_REPLY_BYTES < backstop);
    assert!(MAX_ENV_BASE_BYTES < backstop);
    // The count limits (argv and env entries) are bounded through the
    // byte limit that accompanies each; tags have only a per-entry
    // one, so their product is what has to fit.
    assert!(MAX_SESSION_TAGS * MAX_TAG_BYTES < backstop);
};

/// A message family that carries its own per-operation limits.
///
/// Implemented for every family so a new one cannot be added without
/// deciding; the families with nothing to bound return `Ok(())`.
pub trait Validate {
    /// # Errors
    /// [`WireError::OverLimit`] naming the first field over its cap.
    fn validate(&self) -> Result<(), WireError>;
}

/// Raises [`WireError::OverLimit`] when `len` bytes exceeds `cap`.
///
/// # Errors
/// Returns [`WireError::OverLimit`] naming `field`.
pub const fn check_limit(field: &'static str, len: usize, cap: usize) -> Result<(), WireError> {
    check_unit(field, len, cap, "bytes")
}

/// [`check_limit`] for a cap that counts entries rather than bytes, so
/// the refusal reads in the unit the cap is stated in.
///
/// # Errors
/// [`WireError::OverLimit`] naming `field`.
pub const fn check_count(field: &'static str, count: usize, cap: usize) -> Result<(), WireError> {
    check_unit(field, count, cap, "entries")
}

const fn check_unit(
    field: &'static str,
    len: usize,
    cap: usize,
    unit: &'static str,
) -> Result<(), WireError> {
    if len > cap {
        return Err(WireError::OverLimit {
            field,
            len,
            cap,
            unit,
        });
    }
    Ok(())
}

/// Checks a `u64` wire-width claim against `cap` before narrowing.
///
/// # Errors
/// Returns [`WireError::OverLimit`] naming `field`, with values saturated to `usize`.
pub const fn check_claim(
    field: &'static str,
    claimed: u64,
    cap: u64,
    unit: &'static str,
) -> Result<(), WireError> {
    if claimed > cap {
        return Err(WireError::OverLimit {
            field,
            len: saturate(claimed),
            cap: saturate(cap),
            unit,
        });
    }
    Ok(())
}

const fn saturate(value: u64) -> usize {
    if value > usize::MAX as u64 {
        usize::MAX
    } else {
        value as usize
    }
}

impl Validate for SpawnArgs {
    fn validate(&self) -> Result<(), WireError> {
        check_limit(
            "SpawnArgs.command",
            self.command.len(),
            MAX_SPAWN_PATH_BYTES,
        )?;
        check_limit("SpawnArgs.cwd", self.cwd.len(), MAX_SPAWN_PATH_BYTES)?;
        check_count("SpawnArgs.args", self.args.len(), MAX_SPAWN_ARGV_ENTRIES)?;
        check_limit(
            "SpawnArgs.args bytes",
            self.args.iter().map(String::len).sum(),
            MAX_SPAWN_ARGV_BYTES,
        )?;
        check_count("SpawnArgs.env", self.env.len(), MAX_ENV_BASE_ENTRIES)?;
        check_limit(
            "SpawnArgs.env bytes",
            self.env.iter().map(|(k, v)| k.len() + v.len()).sum(),
            MAX_ENV_BASE_BYTES,
        )?;
        // `env_base` is checked in the daemon (`child_env::sanitize_base`, REQ-912a)
        // to return a non-fatal spawn refusal instead of tearing down the connection.
        check_count("SpawnArgs.tags", self.tags.len(), MAX_SESSION_TAGS)?;
        for tag in &self.tags {
            check_limit("SpawnArgs.tags entry", tag.len(), MAX_TAG_BYTES)?;
        }
        Ok(())
    }
}

impl Validate for RetargetTarget {
    fn validate(&self) -> Result<(), WireError> {
        let carrier_bytes = match &self.carrier {
            RetargetCarrier::DefaultLocal => 0,
            RetargetCarrier::LocalEndpoint(path) => path.len(),
            RetargetCarrier::Ssh {
                destination,
                ssh_args,
            } => destination.len() + ssh_args.iter().map(String::len).sum::<usize>(),
        };
        check_limit(
            "RetargetTarget.carrier",
            carrier_bytes,
            MAX_RETARGET_DESCRIPTOR_BYTES,
        )?;
        match &self.landing {
            RetargetLanding::Attach(prefix) => check_limit(
                "RetargetTarget.landing",
                prefix.len(),
                MAX_RETARGET_DESCRIPTOR_BYTES,
            ),
            RetargetLanding::Create(args) => args.validate(),
        }
    }
}

impl Validate for InputMsg {
    fn validate(&self) -> Result<(), WireError> {
        match self {
            Self::KeyBytes(bytes) => {
                check_limit("Input::KeyBytes", bytes.len(), MAX_RAW_INPUT_BYTES)
            }
            Self::Paste(bytes) => check_limit("Input::Paste", bytes.len(), MAX_PASTE_BYTES),
            Self::Key(event) => {
                if let Key::Character(s) = &event.key {
                    check_limit("Input::Key.character", s.len(), MAX_KEY_CHARACTER_BYTES)?;
                }
                check_limit(
                    "Input::Key.text",
                    event.text.as_ref().map_or(0, String::len),
                    MAX_KEY_TEXT_BYTES,
                )
            }
            _ => Ok(()),
        }
    }
}

impl Validate for SearchToDaemonMsg {
    fn validate(&self) -> Result<(), WireError> {
        match self {
            Self::Query { query, .. } => {
                check_limit("Search::Query.query", query.len(), MAX_SEARCH_PATTERN_BYTES)
            }
        }
    }
}

impl Validate for SessionToDaemonMsg {
    fn validate(&self) -> Result<(), WireError> {
        match self {
            Self::Create { args } => args.validate(),
            // The prefix shares the target slot with `id`, and an empty
            // one encodes as neither field set: the shape the decoder
            // refuses. Checked here so it is refused before a byte
            // leaves rather than as a closed connection.
            Self::Attach {
                target: AttachTarget::Prefix(prefix),
                ..
            } => validate_session_id_prefix(prefix)
                .map(drop)
                .map_err(|_| WireError::MalformedField("SessionAttach.id_prefix")),
            _ => Ok(()),
        }
    }
}

impl Validate for OpsToDaemonMsg {
    fn validate(&self) -> Result<(), WireError> {
        // `Tag` deltas are validated daemon-side to return `TagsUpdated { denied }`
        // rather than tearing down the connection on over-cap inputs.
        match self {
            Self::Spawn { args } => args.validate(),
            Self::Switch {
                target: SwitchTarget::Carrier(target),
                ..
            } => target.validate(),
            _ => Ok(()),
        }
    }
}

impl Validate for PushMsg {
    fn validate(&self) -> Result<(), WireError> {
        match self {
            Self::RetargetHost { target, .. } => target.validate(),
            _ => Ok(()),
        }
    }
}

/// Implements [`Validate`] as a no-op for families without variable-length payloads.
///
/// Allocation-bounding scalars (e.g. geometry, [`MAX_IMAGE_BYTES`], [`MAX_IMAGE_FRAMES`])
/// are validated in `convert/` during ingress conversion.
macro_rules! nothing_to_bound {
    ($($ty:ty),+ $(,)?) => {
        $(impl Validate for $ty {
            fn validate(&self) -> Result<(), WireError> {
                Ok(())
            }
        })+
    };
}

nothing_to_bound!(
    crate::messages::ConnToDaemonMsg,
    crate::messages::ConnToClientMsg,
    crate::messages::GridMsg,
    crate::messages::ImageMsg,
    crate::messages::SessionToClientMsg,
    crate::messages::OpsToClientMsg,
    crate::messages::RegionToDaemonMsg,
    crate::messages::RegionToClientMsg,
    crate::messages::NotifyToDaemonMsg,
    crate::messages::NotifyToClientMsg,
    crate::messages::SearchToClientMsg,
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode};
    use crate::messages::{RequestedDims, SearchOptions};

    /// Every semantic limit is enforced identically on the send side
    /// (`validate`) and after a decode, at the limit and one past it.
    #[test]
    fn each_limit_admits_its_boundary_and_refuses_one_past_it() {
        let key = |n: usize| InputMsg::KeyBytes(vec![0x61; n]);
        let paste = |n: usize| InputMsg::Paste(vec![0x61; n]);
        let query = |n: usize| SearchToDaemonMsg::Query {
            query: "a".repeat(n),
            options: SearchOptions::default(),
        };
        let argv = |n: usize| SessionToDaemonMsg::Create {
            args: SpawnArgs {
                args: vec![String::new(); n],
                ..SpawnArgs::default()
            },
        };
        let cmd = |n: usize| SessionToDaemonMsg::Create {
            args: SpawnArgs {
                command: "a".repeat(n),
                ..SpawnArgs::default()
            },
        };

        assert_pair(&key(MAX_RAW_INPUT_BYTES), &key(MAX_RAW_INPUT_BYTES + 1));
        assert_pair(&paste(MAX_PASTE_BYTES), &paste(MAX_PASTE_BYTES + 1));
        assert_pair(
            &query(MAX_SEARCH_PATTERN_BYTES),
            &query(MAX_SEARCH_PATTERN_BYTES + 1),
        );
        assert_pair(
            &argv(MAX_SPAWN_ARGV_ENTRIES),
            &argv(MAX_SPAWN_ARGV_ENTRIES + 1),
        );
        assert_pair(&cmd(MAX_SPAWN_PATH_BYTES), &cmd(MAX_SPAWN_PATH_BYTES + 1));

        let ssh = |n: usize| PushMsg::RetargetHost {
            target: RetargetTarget {
                carrier: RetargetCarrier::Ssh {
                    destination: "a".repeat(n),
                    ssh_args: Vec::new(),
                },
                landing: RetargetLanding::Attach("ab12".into()),
            },
        };
        assert_pair(
            &ssh(MAX_RETARGET_DESCRIPTOR_BYTES),
            &ssh(MAX_RETARGET_DESCRIPTOR_BYTES + 1),
        );
    }

    /// At the limit: sends and decodes. One past: refused by both,
    /// under [`WireError::OverLimit`].
    fn assert_pair<M>(at: &M, past: &M)
    where
        M: crate::codec::WireCodec + Validate + core::fmt::Debug,
    {
        Validate::validate(at).unwrap();
        decode::<M>(&encode(at)).unwrap();

        assert!(
            matches!(Validate::validate(past), Err(WireError::OverLimit { .. })),
            "send side admitted an over-limit {past:?}"
        );
        let err = decode::<M>(&encode(past)).unwrap_err();
        assert!(
            matches!(
                err,
                crate::codec::CodecError::Wire(WireError::OverLimit { .. })
            ),
            "receive side admitted an over-limit message: {err:?}"
        );
    }

    /// The same `SpawnArgs` refused on `Session::Create` is refused on
    /// `Ops::Spawn`: the two arms are one admission path, so a peer
    /// cannot buy an unbounded argv by dialing in Ops mode.
    #[test]
    fn a_spawn_on_ops_is_capped_exactly_like_a_create() {
        let argv = |n: usize| OpsToDaemonMsg::Spawn {
            args: SpawnArgs {
                args: vec![String::new(); n],
                ..SpawnArgs::default()
            },
        };
        let cmd = |n: usize| OpsToDaemonMsg::Spawn {
            args: SpawnArgs {
                command: "a".repeat(n),
                ..SpawnArgs::default()
            },
        };

        assert_pair(
            &argv(MAX_SPAWN_ARGV_ENTRIES),
            &argv(MAX_SPAWN_ARGV_ENTRIES + 1),
        );
        assert_pair(&cmd(MAX_SPAWN_PATH_BYTES), &cmd(MAX_SPAWN_PATH_BYTES + 1));
    }

    /// A spawn's argv is bounded by total bytes as well as entry count.
    #[test]
    fn a_spawn_argv_under_the_entry_cap_can_still_be_too_many_bytes() {
        let args = SpawnArgs {
            args: vec!["a".repeat(MAX_SPAWN_ARGV_BYTES / 2 + 1); 2],
            ..SpawnArgs::default()
        };
        assert!(matches!(
            args.validate(),
            Err(WireError::OverLimit {
                field: "SpawnArgs.args bytes",
                ..
            })
        ));
    }

    /// A refusal reads in the unit its cap counts: an argv-*entry*
    /// breach reported as bytes sends the caller looking for a payload
    /// it never sent.
    #[test]
    fn a_refusal_names_the_unit_its_cap_counts() {
        assert_eq!(
            check_count("SpawnArgs.args", 4097, 4096)
                .unwrap_err()
                .to_string(),
            "wire field SpawnArgs.args carried 4097 entries, over the 4096 limit"
        );
        assert_eq!(
            check_limit("Input::Paste", 9, 8).unwrap_err().to_string(),
            "wire field Input::Paste carried 9 bytes, over the 8 limit"
        );
        assert_eq!(
            check_claim("ImageHeader.frame", 5, 4, "frames")
                .unwrap_err()
                .to_string(),
            "wire field ImageHeader.frame carried 5 frames, over the 4 limit"
        );
    }

    /// An over-cap `env_base` passes both wire checks: REQ-912a answers
    /// it with the daemon's typed spawn refusal, which costs the caller
    /// one create, where a decode-side refusal would cost the whole
    /// connection.
    #[test]
    fn an_over_cap_env_base_is_left_to_the_spawn_refusal() {
        let msg = SessionToDaemonMsg::Create {
            args: SpawnArgs {
                env_base: Some(vec![(b"K".to_vec(), Vec::new()); MAX_ENV_BASE_ENTRIES + 1]),
                ..SpawnArgs::default()
            },
        };
        msg.validate().unwrap();
        decode::<SessionToDaemonMsg>(&encode(&msg)).unwrap();
    }

    /// A create carrying nothing over-limit is untouched by the checks,
    /// geometry included (geometry has its own admission, REQ-605a).
    #[test]
    fn an_ordinary_create_passes_every_check() {
        SessionToDaemonMsg::Create {
            args: SpawnArgs {
                command: "/bin/sh".into(),
                args: vec!["-lc".into(), "echo hi".into()],
                cwd: "/tmp".into(),
                env: vec![("KEY".into(), "VAL".into())],
                dims: Some(RequestedDims {
                    rows: 70_000,
                    cols: 80,
                    pixel_w: 0,
                    pixel_h: 0,
                }),
                env_base: Some(vec![(b"PATH".to_vec(), b"/bin".to_vec())]),
                tags: vec!["agent".into()],
            },
        }
        .validate()
        .unwrap();
    }
}

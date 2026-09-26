//! Async exchange of the frozen version preface
//! ([`felis_protocol::preface`]) on the raw byte halves. A
//! [`FrameReader`](crate::FrameReader) constructed first would swallow
//! preface bytes into its read-ahead buffer, and `into_inner` hands the
//! carrier back without them.

use std::io;
use std::time::{Duration, Instant};

use felis_protocol::codec;
use felis_protocol::messages::{ConnToClientMsg, ConnToDaemonMsg, Direction};
use felis_protocol::preface::{
    CARRIER_HEADER_LEN, CLIENT_PREFACE_LEN, CarrierBlock, CarrierError, CarrierPayload,
    ClientPreface, DAEMON_PREFACE_LEN, DaemonPreface, PrefaceError, carrier_payload_len,
    confirm_accept,
};
use felis_protocol::{ConnectionMode, MessageKind};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::framing::{FrameReader, FrameWriter};
use crate::local::{Endpoint, ReadHalf, WriteHalf, connect};

#[derive(Debug, Error)]
pub enum PrefaceExchangeError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Preface(#[from] PrefaceError),
    #[error("{0}")]
    Carrier(#[from] CarrierError),
}

/// Flushes: the daemon answers before either side sends a frame, so an
/// unflushed preface deadlocks the handshake.
pub async fn write_client_preface<W: AsyncWrite + Unpin>(
    write_half: &mut W,
    preface: ClientPreface,
) -> Result<(), PrefaceExchangeError> {
    write_half.write_all(&preface.encode()).await?;
    write_half.flush().await?;
    Ok(())
}

pub async fn read_client_preface<R: AsyncRead + Unpin>(
    read_half: &mut R,
) -> Result<ClientPreface, PrefaceExchangeError> {
    let mut buf = [0u8; CLIENT_PREFACE_LEN];
    read_half.read_exact(&mut buf).await?;
    Ok(ClientPreface::decode(&buf)?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientBootstrap {
    /// `None` on a bare `FLIS` stream (a local client, or a relay that
    /// sent no block), and on a block whose format version this build
    /// cannot decode.
    pub carrier: Option<CarrierBlock>,
    /// The format version of a block that arrived and was skipped, for
    /// the caller to report. Skipping costs nothing: the payload is
    /// consumed by its length word either way.
    pub skipped_carrier_version: Option<u16>,
    pub preface: ClientPreface,
}

pub async fn write_carrier_block<W: AsyncWrite + Unpin>(
    write_half: &mut W,
    block: &CarrierBlock,
) -> Result<(), PrefaceExchangeError> {
    write_half.write_all(&block.encode()?).await?;
    write_half.flush().await?;
    Ok(())
}

/// The carrier header and the client preface are both eight bytes, so
/// one read serves either form.
pub async fn read_client_bootstrap<R: AsyncRead + Unpin>(
    read_half: &mut R,
) -> Result<ClientBootstrap, PrefaceExchangeError> {
    let mut head = [0u8; CARRIER_HEADER_LEN];
    read_half.read_exact(&mut head).await?;
    let payload_len = match carrier_payload_len(&head) {
        Ok(len) => len,
        Err(CarrierError::NotCarrier { .. }) => {
            return Ok(ClientBootstrap {
                carrier: None,
                skipped_carrier_version: None,
                preface: ClientPreface::decode(&head)?,
            });
        }
        Err(err) => return Err(err.into()),
    };
    let mut payload = vec![0u8; payload_len as usize];
    read_half.read_exact(&mut payload).await?;
    let (carrier, skipped_carrier_version) = match CarrierBlock::decode_payload(&payload)? {
        CarrierPayload::Block(block) => (Some(block), None),
        CarrierPayload::UnknownVersion(version) => (None, Some(version)),
    };
    Ok(ClientBootstrap {
        carrier,
        skipped_carrier_version,
        preface: read_client_preface(read_half).await?,
    })
}

pub async fn write_daemon_preface<W: AsyncWrite + Unpin>(
    write_half: &mut W,
    reply: impl Into<DaemonPreface>,
) -> Result<(), PrefaceExchangeError> {
    write_half.write_all(&reply.into().encode()).await?;
    write_half.flush().await?;
    Ok(())
}

pub async fn read_daemon_preface<R: AsyncRead + Unpin>(
    read_half: &mut R,
) -> Result<DaemonPreface, PrefaceExchangeError> {
    let mut buf = [0u8; DAEMON_PREFACE_LEN];
    read_half.read_exact(&mut buf).await?;
    Ok(DaemonPreface::decode(&buf)?)
}

/// How long [`probe`] has to reach a terminal outcome. Bounded so a
/// diagnostic that reports on an endpoint, such as a `doctor` row,
/// cannot be held by a listener that accepts and then says nothing.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(2);

/// What one bounded dial at a candidate endpoint established.
///
/// Only [`Self::Absent`] means "nothing is there": the other two
/// non-live variants mean "something may be there and could not be
/// verified", and no caller may read them as cold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The connect proved nothing is listening (`ENOENT`,
    /// `ECONNREFUSED`).
    Absent,
    /// The connect failed for a reason that says nothing about whether
    /// a daemon is there (`EACCES`, `EMFILE`, a connect that timed out).
    Indeterminate { err: String },
    /// Something accepted the connection and then failed to answer as a
    /// felis daemon.
    ConnectedUnverified { why: String },
    /// A felis daemon answered. `detail` carries what it said when that
    /// was not a plain `Welcome`.
    Live { detail: Option<String> },
}

/// `ENOENT` and `ECONNREFUSED` are the only connect failures that prove
/// nothing is listening; every other one leaves the endpoint's state
/// unknown, so treating it as cold would spawn beside a live daemon.
#[must_use]
pub fn connect_error_is_absent(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// One connection to `endpoint`, run to a terminal outcome under
/// `deadline`: connect, exchange the version preface, then ask for a
/// `Welcome` as an observer. Connect-only, so it can never take a live
/// daemon's socket (`docs/explanation/security-model.md` "Daemon IPC").
pub async fn probe(endpoint: impl Into<Endpoint>, deadline: Duration) -> ProbeOutcome {
    let endpoint = endpoint.into();
    let started = Instant::now();
    let halves = match tokio::time::timeout(deadline, connect(endpoint)).await {
        Err(_) => {
            return ProbeOutcome::Indeterminate {
                err: format!(
                    "the connect did not complete within {}s",
                    deadline.as_secs()
                ),
            };
        }
        Ok(Err(err)) => {
            return if connect_error_is_absent(&err) {
                ProbeOutcome::Absent
            } else {
                ProbeOutcome::Indeterminate {
                    err: err.to_string(),
                }
            };
        }
        Ok(Ok(halves)) => halves,
    };
    let remaining = deadline.saturating_sub(started.elapsed());
    match tokio::time::timeout(remaining, handshake_probe(halves)).await {
        Ok(outcome) => outcome,
        Err(_) => ProbeOutcome::ConnectedUnverified {
            why: format!(
                "it accepted the connection but did not finish the felis handshake within {}s",
                deadline.as_secs()
            ),
        },
    }
}

async fn handshake_probe((read_half, write_half): (ReadHalf, WriteHalf)) -> ProbeOutcome {
    let mut write_half = write_half;
    let mut read_half = read_half;
    if let Err(err) = write_client_preface(&mut write_half, ClientPreface::CURRENT).await {
        return ProbeOutcome::ConnectedUnverified {
            why: format!("the preface could not be sent: {err}"),
        };
    }
    let reply = match read_daemon_preface(&mut read_half).await {
        Ok(reply) => reply,
        Err(err) => {
            return ProbeOutcome::ConnectedUnverified {
                why: format!("it answered no readable felis preface: {err}"),
            };
        }
    };
    // Every negotiation verdict short of an accept is an answer from a
    // live daemon: a refusal, a status only a newer felis knows, and an
    // accept naming a major this build never offered.
    let effective_minor = match confirm_accept(ClientPreface::CURRENT, reply) {
        Ok(minor) => minor,
        Err(err) => {
            return ProbeOutcome::Live {
                detail: Some(err.to_string()),
            };
        }
    };

    let mut writer = FrameWriter::new(write_half, effective_minor);
    if let Err(err) = writer
        .send(&ConnToDaemonMsg::Hello {
            mode: ConnectionMode::Observer,
            pull_paced: false,
        })
        .await
    {
        return ProbeOutcome::ConnectedUnverified {
            why: format!("it accepted the preface and then refused the handshake frame: {err}"),
        };
    }
    let mut reader = FrameReader::new(read_half);
    let frame = match reader.next_frame().await {
        Ok(Some(frame)) => frame,
        Ok(None) => {
            return ProbeOutcome::ConnectedUnverified {
                why: "it accepted the preface and then closed without answering".to_owned(),
            };
        }
        Err(err) => {
            return ProbeOutcome::ConnectedUnverified {
                why: format!("its reply was not a readable felis frame: {err}"),
            };
        }
    };
    // The kind before the body: a peer that writes a `Welcome` payload
    // under another family's kind has not answered the handshake, and
    // decoding the body alone would read it as one.
    if frame.kind != MessageKind::Conn.as_u16() {
        return ProbeOutcome::ConnectedUnverified {
            why: format!(
                "its first frame arrived on kind {}, not the handshake family",
                frame.kind
            ),
        };
    }
    // The decode below reads past a daemon-bound arm as an unknown
    // field, so a reply carrying one would pass for a handshake.
    if let Ok(Some(arm)) = codec::arm_in(MessageKind::Conn, Direction::ToDaemon, &frame.body) {
        return ProbeOutcome::ConnectedUnverified {
            why: format!("its first frame carried {}, a daemon-bound arm", arm.name),
        };
    }
    match codec::decode::<ConnToClientMsg>(&frame.body) {
        Ok(ConnToClientMsg::Welcome { .. }) => ProbeOutcome::Live { detail: None },
        // A full daemon is a live daemon, and so is one that refuses
        // this connection's role.
        Ok(ConnToClientMsg::Refused { reason, detail }) => ProbeOutcome::Live {
            detail: Some(format!("refused this probe ({reason:?}): {detail}")),
        },
        Ok(other) => ProbeOutcome::ConnectedUnverified {
            why: format!("its first frame was {other:?}, not a handshake reply"),
        },
        Err(err) => ProbeOutcome::ConnectedUnverified {
            why: format!("its first frame did not decode: {err}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use felis_protocol::preface::{
        DaemonAccept, MAX_CARRIER_PAYLOAD_BYTES, NegotiationError, PROTOCOL_MAJOR, PROTOCOL_MINOR,
        confirm_accept,
    };
    use tokio::io::duplex;

    use super::*;

    #[tokio::test]
    async fn a_preface_exchange_meets_in_the_middle() {
        let (mut client, mut daemon) = duplex(64);
        write_client_preface(&mut client, ClientPreface::CURRENT)
            .await
            .unwrap();
        let seen = read_client_preface(&mut daemon).await.unwrap();
        assert_eq!(seen.major, PROTOCOL_MAJOR);
        assert_eq!(seen.minor, PROTOCOL_MINOR);

        write_daemon_preface(&mut daemon, DaemonAccept::select(seen).unwrap())
            .await
            .unwrap();
        assert_eq!(
            read_daemon_preface(&mut client).await.unwrap(),
            DaemonPreface::Accept {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
            }
        );
    }

    /// The client half of negotiation over a real byte stream: the
    /// reply is decoded leniently and then confirmed exactly against
    /// what was offered.
    async fn negotiate(reply: DaemonPreface) -> Result<u16, NegotiationError> {
        let (mut client, mut daemon) = duplex(64);
        write_client_preface(&mut client, ClientPreface::CURRENT)
            .await
            .unwrap();
        let seen = read_client_preface(&mut daemon).await.unwrap();
        assert_eq!(seen, ClientPreface::CURRENT);
        write_daemon_preface(&mut daemon, reply).await.unwrap();
        let reply = read_daemon_preface(&mut client).await.unwrap();
        confirm_accept(ClientPreface::CURRENT, reply)
    }

    /// An accept naming a major the client did not offer decodes as an
    /// accept and is then refused by the negotiator, before any frame.
    #[tokio::test]
    async fn an_accept_naming_an_unoffered_major_is_refused_after_decoding() {
        assert_eq!(
            negotiate(DaemonPreface::Accept {
                major: PROTOCOL_MAJOR + 8,
                minor: PROTOCOL_MINOR,
            })
            .await,
            Err(NegotiationError::AcceptedUnofferedMajor {
                offered: PROTOCOL_MAJOR,
                accepted: PROTOCOL_MAJOR + 8,
            }),
        );
    }

    /// The offered major with a daemon minor older than, equal to, and
    /// newer than the client's: the effective minor is the lower one.
    #[tokio::test]
    async fn an_accept_of_the_offered_major_yields_the_effective_minor() {
        for (daemon_minor, want) in [
            (0, 0),
            (PROTOCOL_MINOR, PROTOCOL_MINOR),
            (PROTOCOL_MINOR + 5, PROTOCOL_MINOR),
        ] {
            assert_eq!(
                negotiate(DaemonPreface::Accept {
                    major: PROTOCOL_MAJOR,
                    minor: daemon_minor,
                })
                .await,
                Ok(want),
                "daemon minor {daemon_minor}"
            );
        }
    }

    #[tokio::test]
    async fn a_refusal_and_an_unknown_status_keep_their_words() {
        assert_eq!(
            negotiate(DaemonPreface::Refuse {
                min_major: PROTOCOL_MAJOR + 1,
                max_major: PROTOCOL_MAJOR + 2,
            })
            .await,
            Err(NegotiationError::Refused {
                min: PROTOCOL_MAJOR + 1,
                max: PROTOCOL_MAJOR + 2,
            }),
        );
        assert_eq!(
            negotiate(DaemonPreface::Unknown {
                status: 7,
                words: [9, 9],
            })
            .await,
            Err(NegotiationError::Unknown {
                status: 7,
                words: [9, 9],
            }),
        );
    }

    #[tokio::test]
    async fn a_short_preface_is_an_io_error() {
        let (mut client, mut daemon) = duplex(64);
        client.write_all(b"FLI").await.unwrap();
        drop(client);
        let err = read_client_preface(&mut daemon).await.unwrap_err();
        assert!(matches!(err, PrefaceExchangeError::Io(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn a_non_felis_peer_is_rejected_by_magic() {
        let (mut client, mut daemon) = duplex(64);
        client.write_all(b"GET / HT").await.unwrap();
        let err = read_client_preface(&mut daemon).await.unwrap_err();
        assert!(
            matches!(
                err,
                PrefaceExchangeError::Preface(PrefaceError::NotFelis { .. })
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_bare_preface_bootstraps_with_no_carrier() {
        let (mut client, mut daemon) = duplex(64);
        write_client_preface(&mut client, ClientPreface::CURRENT)
            .await
            .unwrap();
        let opened = read_client_bootstrap(&mut daemon).await.unwrap();
        assert_eq!(opened.carrier, None);
        assert_eq!(opened.preface, ClientPreface::CURRENT);
    }

    #[tokio::test]
    async fn a_carrier_block_precedes_an_untouched_preface() {
        let (mut relay, mut daemon) = duplex(256);
        let block = CarrierBlock {
            env: vec![(b"SSH_AUTH_SOCK".to_vec(), b"/tmp/ssh-XXXX/agent.7".to_vec())],
        };
        write_carrier_block(&mut relay, &block).await.unwrap();
        write_client_preface(&mut relay, ClientPreface::CURRENT)
            .await
            .unwrap();

        let opened = read_client_bootstrap(&mut daemon).await.unwrap();
        assert_eq!(opened.carrier, Some(block));
        assert_eq!(opened.preface, ClientPreface::CURRENT);
    }

    #[tokio::test]
    async fn a_truncated_carrier_header_is_an_io_error() {
        let (mut relay, mut daemon) = duplex(64);
        let bytes = CarrierBlock { env: Vec::new() }.encode().unwrap();
        relay
            .write_all(&bytes[..CARRIER_HEADER_LEN - 1])
            .await
            .unwrap();
        drop(relay);
        let err = read_client_bootstrap(&mut daemon).await.unwrap_err();
        assert!(matches!(err, PrefaceExchangeError::Io(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn a_truncated_carrier_block_is_an_io_error() {
        let (mut relay, mut daemon) = duplex(64);
        let mut bytes = CarrierBlock {
            env: vec![(b"PATH".to_vec(), b"/bin".to_vec())],
        }
        .encode()
        .unwrap();
        bytes.truncate(bytes.len() - 2);
        relay.write_all(&bytes).await.unwrap();
        drop(relay);
        let err = read_client_bootstrap(&mut daemon).await.unwrap_err();
        assert!(matches!(err, PrefaceExchangeError::Io(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn a_malformed_carrier_block_is_typed() {
        let (mut relay, mut daemon) = duplex(64);
        write_carrier_bytes(&mut relay, &[0x00, 0x01, 0x00, 0x00, 0x00, 0x01]).await;
        let err = read_client_bootstrap(&mut daemon).await.unwrap_err();
        assert!(
            matches!(err, PrefaceExchangeError::Carrier(CarrierError::Truncated)),
            "got {err:?}"
        );
    }

    async fn write_carrier_bytes<W: AsyncWrite + Unpin>(write_half: &mut W, payload: &[u8]) {
        write_half.write_all(b"FRLY").await.unwrap();
        write_half
            .write_all(&u32::try_from(payload.len()).unwrap().to_be_bytes())
            .await
            .unwrap();
        write_half.write_all(payload).await.unwrap();
    }

    /// A block written in a format version this build has no decoder
    /// for is skipped by its length word, and the preface behind it
    /// arrives untouched.
    #[tokio::test]
    async fn an_unknown_carrier_version_is_skipped_and_the_preface_resumes() {
        let (mut relay, mut daemon) = duplex(256);
        write_carrier_bytes(&mut relay, b"\x00\x09 a later format's payload").await;
        write_client_preface(&mut relay, ClientPreface::CURRENT)
            .await
            .unwrap();

        let opened = read_client_bootstrap(&mut daemon).await.unwrap();
        assert_eq!(opened.carrier, None);
        assert_eq!(opened.skipped_carrier_version, Some(9));
        assert_eq!(opened.preface, ClientPreface::CURRENT);
    }

    /// A relay that predates the format version word writes its entry
    /// count where the version belongs, so its payload reads as version
    /// 0 and closes the connection.
    #[tokio::test]
    async fn a_versionless_carrier_payload_closes_the_connection() {
        let (mut relay, mut daemon) = duplex(256);
        let mut versionless = Vec::new();
        versionless.extend_from_slice(&1u32.to_be_bytes());
        versionless.extend_from_slice(&4u32.to_be_bytes());
        versionless.extend_from_slice(b"PATH");
        versionless.extend_from_slice(&4u32.to_be_bytes());
        versionless.extend_from_slice(b"/bin");
        write_carrier_bytes(&mut relay, &versionless).await;
        write_client_preface(&mut relay, ClientPreface::CURRENT)
            .await
            .unwrap();

        let err = read_client_bootstrap(&mut daemon).await.unwrap_err();
        assert!(
            matches!(
                err,
                PrefaceExchangeError::Carrier(CarrierError::ZeroVersion)
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn an_over_cap_carrier_length_is_refused_before_the_read() {
        let (mut relay, mut daemon) = duplex(64);
        relay.write_all(b"FRLY").await.unwrap();
        relay
            .write_all(&(MAX_CARRIER_PAYLOAD_BYTES + 1).to_be_bytes())
            .await
            .unwrap();
        let err = read_client_bootstrap(&mut daemon).await.unwrap_err();
        assert!(
            matches!(
                err,
                PrefaceExchangeError::Carrier(CarrierError::OverCap { .. })
            ),
            "got {err:?}"
        );
    }

    /// Only these two prove the endpoint is cold; everything else has
    /// to stay unverified or a caller would spawn beside a live daemon.
    #[test]
    fn only_enoent_and_econnrefused_prove_an_endpoint_is_cold() {
        for kind in [io::ErrorKind::NotFound, io::ErrorKind::ConnectionRefused] {
            assert!(
                connect_error_is_absent(&io::Error::new(kind, "x")),
                "{kind}"
            );
        }
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::Other,
        ] {
            assert!(
                !connect_error_is_absent(&io::Error::new(kind, "x")),
                "{kind}"
            );
        }
    }

    #[cfg(unix)]
    /// The whole autospawn decision rests on this: anything but these
    /// two kinds leaves the endpoint's state unknown.
    #[test]
    fn only_not_found_and_refused_prove_nothing_is_listening() {
        for kind in [io::ErrorKind::NotFound, io::ErrorKind::ConnectionRefused] {
            assert!(connect_error_is_absent(&io::Error::from(kind)), "{kind:?}");
        }
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::TimedOut,
            io::ErrorKind::Other,
        ] {
            assert!(!connect_error_is_absent(&io::Error::from(kind)), "{kind:?}");
        }
        // `EMFILE` has no named kind, so it must not fall into one.
        #[cfg(unix)]
        assert!(!connect_error_is_absent(&io::Error::from_raw_os_error(24)));
    }

    #[tokio::test]
    async fn a_path_with_nothing_on_it_probes_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            probe(tmp.path().join("nothing.sock"), PROBE_DEADLINE).await,
            ProbeOutcome::Absent
        );
    }

    /// A socket parent must be a `0700` directory this uid owns
    /// (REQ-107), and `TempDir` follows the process umask.
    #[cfg(unix)]
    fn private_dir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        tmp
    }

    /// A listener that accepts and then says nothing must not hold the
    /// probe, and must never read as cold.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_silent_listener_probes_connected_but_unverified() {
        let tmp = private_dir();
        let path = tmp.path().join("silent.sock");
        let listener = crate::local::Listener::bind(&Endpoint::unix(path.clone())).unwrap();
        let accepting = tokio::spawn(async move { listener.accept().await });

        let outcome = probe(path, Duration::from_millis(200)).await;
        assert!(
            matches!(outcome, ProbeOutcome::ConnectedUnverified { .. }),
            "got {outcome:?}"
        );
        drop(accepting.await.unwrap());
    }

    /// Probes a listener that accepts the preface and answers the
    /// handshake with one frame of `kind` holding `body`.
    #[cfg(unix)]
    async fn probe_answered_with(kind: MessageKind, body: Vec<u8>) -> ProbeOutcome {
        use felis_protocol::frame::Frame;

        let tmp = private_dir();
        let path = tmp.path().join("answering.sock");
        let listener = crate::local::Listener::bind(&Endpoint::unix(path.clone())).unwrap();
        let serving = tokio::spawn(async move {
            let stream = listener.accept().await.expect("accept");
            let (read_half, mut write_half) = crate::local::server_split(stream);
            let mut read_half = read_half;
            read_client_preface(&mut read_half).await.expect("preface");
            write_daemon_preface(
                &mut write_half,
                DaemonPreface::Accept {
                    major: PROTOCOL_MAJOR,
                    minor: PROTOCOL_MINOR,
                },
            )
            .await
            .expect("accept the preface");
            let mut reader = FrameReader::new(read_half);
            drop(reader.next_frame().await);
            let answer = Frame {
                kind: kind.as_u16(),
                body: &body,
            }
            .encode()
            .expect("a small body encodes");
            write_half.write_all(&answer).await.expect("write");
            write_half.flush().await.expect("flush");
            std::future::pending::<()>().await;
        });

        let outcome = probe(path, PROBE_DEADLINE).await;
        serving.abort();
        outcome
    }

    /// A `Welcome` body under another family's kind is not an answer to
    /// the handshake; reading it as one would let any peer that can
    /// replay the bytes pass as a daemon.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_welcome_body_on_a_foreign_kind_probes_connected_but_unverified() {
        let body = codec::encode(&ConnToClientMsg::Welcome { identity: None });
        let outcome = probe_answered_with(MessageKind::Ops, body).await;
        assert!(
            matches!(outcome, ProbeOutcome::ConnectedUnverified { .. }),
            "got {outcome:?}"
        );
    }

    /// A `Welcome` sharing its body with a daemon-bound `Hello` is no
    /// handshake reply either, although the client half alone reads it
    /// as one.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_welcome_carrying_a_daemon_bound_arm_probes_connected_but_unverified() {
        let welcome = codec::encode(&ConnToClientMsg::Welcome { identity: None });
        let hello = codec::encode(&ConnToDaemonMsg::Hello {
            mode: ConnectionMode::Ops,
            pull_paced: false,
        });
        let outcome = probe_answered_with(MessageKind::Conn, [welcome, hello].concat()).await;
        assert!(
            matches!(&outcome, ProbeOutcome::ConnectedUnverified { why } if why.contains("Conn::Hello")),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_non_felis_peer_is_still_rejected_by_magic() {
        let (mut client, mut daemon) = duplex(64);
        client.write_all(b"GET / HT").await.unwrap();
        let err = read_client_bootstrap(&mut daemon).await.unwrap_err();
        assert!(
            matches!(
                err,
                PrefaceExchangeError::Preface(PrefaceError::NotFelis { .. })
            ),
            "got {err:?}"
        );
    }
}

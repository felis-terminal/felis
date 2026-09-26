//! Connection plumbing for the headless CLI verbs: the one place that
//! decides where a verb connects, so `--host`, `--ssh-arg`, and
//! `--socket` behave identically everywhere. Cross-host CLI rides the
//! SSH stdio carrier (docs/reference/ipc.md "CLI clients").

use std::path::Path;

use anyhow::Result;
use felis_client_core::local_socket::{SocketSource, resolve_local_socket_source};
use felis_client_core::{
    Carrier, CarrierConnection, ConnectError, Connection, Offer, Reconnector, RemoteSpawn,
    connect_carrier,
};
use felis_protocol::MessageKind;
use felis_protocol::messages::{GridMsg, RefusalReason};
use felis_transport::{Delivery, DriverError, Incoming};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::cli_output::{ErrorKind, Reporter};

/// A dial target and where its address came from. `Reconnector` carries
/// only the address, and an explicit or stamped path can equal the
/// platform default, so provenance has to travel beside it for the one
/// verb that reasons about the default endpoint's surroundings.
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    pub(crate) target: Reconnector,
    /// `None` under `--host`: the SSH carrier has no local provenance.
    pub(crate) local_source: Option<SocketSource>,
}

/// Resolve the connection target from `--host` or the local socket.
///
/// The descriptor carries [`Offer::ops`]; [`Dial`] re-stamps its own mode on
/// each open because `notifications subscribe` dials as an observer.
pub(crate) fn resolve(
    host: Option<&str>,
    ssh_args: &[String],
    socket: Option<&Path>,
) -> Result<Resolved> {
    let (carrier, local_source) = if let Some(host) = host {
        (
            Carrier::Ssh {
                destination: host.to_owned(),
                ssh_args: ssh_args.to_vec(),
            },
            None,
        )
    } else {
        let resolved = resolve_local_socket_source(socket)?;
        (Carrier::Local(resolved.path.into()), Some(resolved.source))
    };
    Ok(Resolved {
        target: Reconnector {
            carrier,
            offer: Offer::ops(),
        },
        local_source,
    })
}

/// How one verb reaches the daemon: its connection mode, and whether a
/// cold socket is a failure or an invitation to start one. The "which
/// verbs auto-spawn" rule is docs/reference/ipc.md "CLI clients".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dial {
    Ops,
    Observer,
    /// `sessions spawn`, the one verb that auto-spawns the daemon:
    /// creating a session implies wanting a daemon to hold it, unlike
    /// the read-side verbs' "no silent resurrection" posture. It dials
    /// through [`Reconnector::dial_launch`], which autospawns on the
    /// local carrier only and leaves the remote case to the SSH relay.
    OpsOrSpawn,
}

impl Dial {
    /// Report open failures through [`Reporter`] and yield the CLI exit code.
    ///
    /// Uses the reporter so a stream verb emits a terminal record instead of
    /// leaving a consumer unable to distinguish errors from an empty stream.
    pub(crate) async fn open(
        self,
        target: &Reconnector,
        out: &Reporter,
    ) -> Result<CarrierConnection, i32> {
        let dialing = Reconnector {
            carrier: target.carrier.clone(),
            offer: self.offer(),
        };
        // The daemon's own refusal is lifted out before the error is
        // rendered: past that point every failure is a string, and a
        // full daemon and a dead one would share one exit code.
        let opened = match self {
            Self::Ops | Self::Observer => dial(&dialing, self.remote_spawn())
                .await
                .map_err(|err| (refusal(&err), err.to_string())),
            // Through `anyhow`: only the alternate chain rendering
            // appends the typed source that says why the step failed.
            Self::OpsOrSpawn => dialing
                .dial_launch(self.remote_spawn())
                .await
                .map_err(|err| (refusal(&err), format!("{:#}", anyhow::Error::new(err)))),
        };
        match opened {
            Ok(conn) => Ok(conn),
            Err((Some((reason, detail)), _)) => Err(out.fail(
                ErrorKind::from_refusal(reason),
                format!("the felis daemon refused a connection: {detail}"),
            )),
            Err((None, err)) => Err(out.fail(
                ErrorKind::DaemonUnreachable,
                unreachable_message(target, err),
            )),
        }
    }

    const fn offer(self) -> Offer {
        match self {
            Self::Ops | Self::OpsOrSpawn => Offer::ops(),
            Self::Observer => Offer::observer(),
        }
    }

    /// The verb's half of the auto-spawn matrix
    /// (docs/reference/cli.md "Auto-spawning"): intent decides, the
    /// carrier does not. [`Self::open`] hands it to the dial, so over
    /// SSH it becomes the `--no-spawn` flag on the relay command.
    pub(crate) const fn remote_spawn(self) -> RemoteSpawn {
        match self {
            Self::Ops | Self::Observer => RemoteSpawn::Refuse,
            Self::OpsOrSpawn => RemoteSpawn::Allow,
        }
    }
}

/// The raw open, for callers that inspect the failure themselves
/// (`probe` in `cli_version.rs`, the silent completions roster). Each
/// names its own [`RemoteSpawn`], so a verb's place in the auto-spawn
/// matrix is readable at its dial site.
pub(crate) async fn dial(
    target: &Reconnector,
    remote_spawn: RemoteSpawn,
) -> Result<CarrierConnection, ConnectError> {
    connect_carrier(target.carrier.clone(), target.offer, remote_spawn).await
}

fn refusal(err: &(dyn std::error::Error + 'static)) -> Option<(RefusalReason, String)> {
    felis_client_core::refusal_detail(err).map(|(reason, detail)| (reason, detail.to_owned()))
}

/// Names the carrier so the remedy is obvious (a dead local socket vs.
/// an unreachable SSH host).
pub(crate) fn unreachable_message(target: &Reconnector, err: impl std::fmt::Display) -> String {
    match &target.carrier {
        Carrier::Local(socket) => format!(
            "no daemon at {socket}: {err}\n(start one with `felis sessions spawn` or by opening a \
             felis window)"
        ),
        Carrier::Ssh { destination, .. } => {
            format!("cannot reach the felis daemon on {destination}: {err}")
        }
    }
}

/// Why [`drain_rehydrate`] stopped short of the burst's end. Each
/// surface words these itself, so the mapping to its error vocabulary
/// stays at the caller.
#[derive(Debug)]
pub(crate) enum RehydrateError {
    Closed,
    Read(ConnectError),
    TimedOut,
    Corrupt(DriverError),
}

/// Consume the visible-grid burst the daemon sends on attach, up to
/// its `RehydrateEnd`. A verb that reads regions directly has no use
/// for the burst, but a request sent before it ends would race it.
pub(crate) async fn drain_rehydrate<R, W>(conn: &mut Connection<R, W>) -> Result<(), RehydrateError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // A real daemon ships the burst promptly, so a longer wait means
    // something is wrong.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // Through the connection, not the raw reader: frames a verb
        // parked while its own reply was outstanding are queued there.
        let frame = match tokio::time::timeout_at(deadline, conn.next_frame()).await {
            Ok(Ok(Some(frame))) => frame,
            Ok(Ok(None)) => return Err(RehydrateError::Closed),
            Ok(Err(err)) => return Err(RehydrateError::Read(err)),
            Err(_elapsed) => return Err(RehydrateError::TimedOut),
        };
        // Every frame goes through the driver, burst or not: it judges
        // the connection's phase and would catch a daemon sending
        // something this attach never asked for.
        let incoming = conn
            .driver
            .classify(&frame)
            .map_err(RehydrateError::Corrupt)?;
        // Through the driver rather than a raw decode: only `decode`
        // enforces the burst's own arm rows, and reading the body
        // around it would exempt the frames this loop reads most.
        if let Incoming::Payload(payload) = incoming {
            if payload.kind == MessageKind::Grid {
                let delivery = conn
                    .driver
                    .decode::<GridMsg>(&payload)
                    .map_err(RehydrateError::Corrupt)?;
                if matches!(delivery, Delivery::Deliver(ref grid) if grid.msg == GridMsg::RehydrateEnd)
                {
                    return Ok(());
                }
            } else {
                conn.driver
                    .admit_drained(&payload)
                    .map_err(RehydrateError::Corrupt)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn resolve_prefers_host_as_the_ssh_relay_carrier() {
        match resolve(
            Some("user@host"),
            &["-p".to_string(), "2222".to_string()],
            None,
        )
        .unwrap()
        .target
        .carrier
        {
            Carrier::Ssh {
                destination,
                ssh_args,
            } => {
                assert_eq!(destination, "user@host");
                assert_eq!(ssh_args, vec!["-p", "2222"]);
            }
            Carrier::Local(_) => panic!("--host must select the SSH carrier"),
        }
    }

    #[test]
    fn resolve_uses_an_explicit_socket_override_when_no_host() {
        let socket = PathBuf::from("/tmp/felis-resolve-test.sock");
        let resolved = resolve(None, &[], Some(&socket)).unwrap();
        assert!(
            matches!(resolved.target.carrier, Carrier::Local(_)),
            "no --host must stay on the local carrier"
        );
        assert_eq!(resolved.target.carrier.local_socket(), Some(socket));
        assert_eq!(resolved.local_source, Some(SocketSource::Explicit));
    }

    #[test]
    fn dial_policies_declare_their_handshake_mode() {
        assert_eq!(Dial::Ops.offer(), Offer::ops());
        assert_eq!(Dial::OpsOrSpawn.offer(), Offer::ops());
        assert_eq!(Dial::Observer.offer(), Offer::observer());
    }
}

#![cfg_attr(not(feature = "daemon-tests"), allow(unused))]

use felis_protocol::frame::Frame;

#[cfg(unix)]
use std::{num::NonZeroU32, sync::Arc};

#[cfg(all(unix, feature = "daemon-tests"))]
use felis_daemon::{SessionPool, serve::DaemonCaps, serve::default_session_factory, serve_unix};
#[cfg(unix)]
use tempfile::TempDir;

#[cfg(all(unix, feature = "daemon-tests"))]
use crate::test_support::{daemon_running, daemon_with_pool};
#[cfg(unix)]
use crate::test_support::{private_dir, serve_bound};
#[cfg(unix)]
use tokio::sync::Mutex;

use super::*;
use std::time::Duration;

use super::{
    ConnectError, Connection,
    carrier::{CarrierConnection, Offer, RemoteSpawn, connect_relay_armed, relay_command},
    check_arm_minor, check_mode_minor,
    requests::{
        AttachIntent, ForceDetachReq, SessionRefusalContext, SwitchSessionReq, TagReq, admits,
    },
};
use felis_protocol::{
    ConnectionMode,
    messages::{
        AttachFailure, AttachRefusal, ConnToClientMsg, ConnToDaemonMsg, Correlation, CreateFailure,
        Directed, OpsToClientMsg, OpsToDaemonMsg, RefusalReason, RequestId, ResolvedId,
        SessionToClientMsg, SessionToDaemonMsg, StreamErrorReason, StreamId, Subject, SwitchDenied,
        SwitchScope, SwitchTarget,
    },
    preface,
};
use felis_transport::{DriverError, FrameReader, FrameWriter, PrefaceExchangeError};
use tokio::process::Command;

#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
use super::carrier::{
    BoundedDialError, Carrier, connect, connect_carrier_with_retry, dial_bounded,
};
#[cfg(unix)]
use felis_protocol::{
    messages::{RetargetTarget, SessionInfo, SpawnArgs},
    preface::ClientPreface,
};
#[cfg(unix)]
use felis_transport::{Delivery, Incoming, TransportError, retry::RetryPolicy};

#[cfg(unix)]
use felis_grid::decode_row;
#[cfg(unix)]
use felis_protocol::{MessageKind, codec, messages::GridMsg};

#[cfg(unix)]
mod daemon_round_trip;
mod handshake;
mod minor_gates;
mod scripted_replies;
#[cfg(unix)]
mod session_exit;
mod ssh_relay;

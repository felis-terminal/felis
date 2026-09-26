//! Headless client core shared across felis frontends.
//!
//! Must compile without GPU, font, or window dependencies. The `native` feature
//! gates tokio and transport; `default-features = false` leaves the portable
//! core compiling to wasm32.

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod action;
pub mod clipboard;
#[cfg(feature = "native")]
pub mod config;
#[cfg(feature = "native")]
pub mod config_watcher;
pub mod confirm;
#[cfg(feature = "native")]
pub mod connector;
#[cfg(feature = "native")]
pub mod cursor_blink;
pub mod cursor_trail;
#[cfg(feature = "native")]
pub mod dial;
pub mod doctor;
#[cfg(feature = "native")]
pub mod env_base;
pub mod hyperlink;
pub mod image_shadow;
pub mod keymap;
#[cfg(feature = "native")]
pub mod local_socket;
#[cfg(feature = "native")]
pub mod outgoing;
#[cfg(feature = "native")]
pub mod pipe;
pub mod pull;
pub mod redraw;
pub mod renderer_effect;
#[cfg(feature = "native")]
pub mod roster;
pub mod selection;
pub mod session_id;
#[cfg(feature = "native")]
pub mod shader_clock;
pub mod shadow;
#[cfg(feature = "native")]
pub mod spawn;
#[cfg(feature = "native")]
pub mod stream;
pub mod viewport;

pub use action::{
    Action, ClipboardScope, Escapes, FontSizeStep, IpcAction, PipeRegionSource, PipeTarget,
    ScrollStep, SwitchDirection,
};
pub use clipboard::{Clipboard, ClipboardError, InMemoryClipboard};
#[cfg(feature = "native")]
pub use config::{
    ConfigDiagnostics, ConfigDocument, ConfigSource, Diagnostic, DiagnosticKind, EffectiveConfig,
    LoadError, Severity, config_path,
};
#[cfg(feature = "native")]
pub use connector::{
    AttachIntent, BoundedDialError, Carrier, CarrierConnection, CarrierReader, CarrierWriter,
    ConnectError, Connection, DaemonStatus, Offer, RemoteSpawn, SessionRefusalContext, SwitchReply,
    admits, connect, connect_carrier, dial_bounded, refusal_detail,
};
#[cfg(feature = "native")]
pub use dial::{
    DialError, DialIntent, DialedConnection, LANDING_REMOTE_SPAWN, Landing, LaunchCwdError,
    RECONNECT_ATTEMPT_TIMEOUT, ROSTER_FETCH_TIMEOUT, ReconnectError, Reconnector, dial_and_land,
    dial_and_land_within, fetch_roster, launch_args, reconnector_for_target, redial_session,
    spawn_args_from_cli,
};
pub use felis_protocol::{ImageId, PlacementId};
pub use hyperlink::{ActivationRejection, ActivationTarget, SchemeClass};
pub use image_shadow::{
    ClientImage, ClientPlacement, ImageShadow, ImageShadowError, VirtualPlacement,
};
pub use keymap::{BindingValue, Chord, FKey, KeyCode, Keymap, Modifiers, NamedKey, PipeSink};
#[cfg(feature = "native")]
pub use outgoing::{
    CoalesceKey, OutgoingFrame, OutgoingFull, OutgoingQueue, coalesce_key, seals_key,
};
pub use selection::{GridPos, Selection, SelectionMode};
pub use shadow::{ShadowError, ShadowScreen};
#[cfg(feature = "native")]
pub use spawn::{SpawnConnectError, connect_or_spawn_daemon};
#[cfg(feature = "native")]
pub use stream::{OpenFrom, OpenStreamError, begin_stream, open_stream};

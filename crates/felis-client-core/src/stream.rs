//! Opening a stream from the client side, in one place
//! (`docs/reference/ipc.md` "Correlation, requests, and streams"). A
//! stream id is positional, so the allocation, the phase transition it
//! may need, and the envelope the opener travels under belong to one
//! call rather than to each verb.

use felis_protocol::{
    codec::{Correlated, WireCodec},
    messages::{Correlation, Directed, StreamId},
    minor::MinorGated,
};
use felis_transport::{ClientDriver, DriverError, FrameWriter, TransportError};
use thiserror::Error;
use tokio::io::AsyncWrite;

/// The phase the opening request is written from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenFrom {
    /// `Region::Rows`, `Search::Query`: a session connection that is
    /// already attached and stays there.
    Attached,
    /// `Notify::Subscribe`, which moves the connection to `Observing`
    /// before the opener is written: after the send, the transition
    /// would race the ack and refuse it as out of phase.
    Setup,
}

/// Why an open never reached a live stream.
#[derive(Debug, Error)]
pub enum OpenStreamError<E> {
    /// The body is unsendable as written. No id was issued and no byte
    /// left, so the connection is still usable.
    #[error("the opening request is not sendable: {0}")]
    Invalid(TransportError),
    /// `begin` refused: the [`begin_stream`] call, or whatever
    /// registration the caller's reader needs. No byte left.
    #[error("{0}")]
    Begin(E),
    /// The write failed after the id was issued. The caller owns the
    /// teardown: this connection can open nothing further.
    #[error("write the opening request: {0}")]
    Write(TransportError),
}

/// Issue the next unopened stream id, with the phase transition the
/// opener needs. Only [`open_stream`] should call it.
///
/// # Errors
/// [`DriverError::SequenceExhausted`] when no id is left.
pub fn begin_stream(driver: &mut ClientDriver, from: OpenFrom) -> Result<StreamId, DriverError> {
    let id = driver.open_stream()?;
    if from == OpenFrom::Setup {
        driver.observing();
    }
    Ok(id)
}

/// Write `msg` as the opener of the stream `begin` allocates; nothing
/// may interleave between them.
///
/// # Errors
/// See [`OpenStreamError`].
pub async fn open_stream<M, W, E>(
    writer: &mut FrameWriter<W>,
    msg: &M,
    begin: impl FnOnce() -> Result<StreamId, E>,
) -> Result<StreamId, OpenStreamError<E>>
where
    M: Correlated + MinorGated + Directed + Sync,
    W: AsyncWrite + Unpin,
{
    // Validated before an id is issued: a frame refused after the
    // allocation would leave the connection a skipped id it cannot
    // recover from.
    WireCodec::validate(msg).map_err(|err| OpenStreamError::Invalid(err.into()))?;
    let id = begin().map_err(OpenStreamError::Begin)?;
    writer
        .send_correlated(msg, Correlation::stream(id))
        .await
        .map_err(OpenStreamError::Write)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use felis_protocol::{
        ConnectionMode, MessageKind,
        codec::peek_correlation,
        messages::{
            MAX_SEARCH_PATTERN_BYTES, RegionSource, RegionToDaemonMsg, SearchOptions,
            SearchToDaemonMsg, StreamId,
        },
    };
    use felis_transport::{FrameReader, FrameWriter, OwnedFrame, Phase};

    use crate::connector::{ConnectError, Connection};

    /// A client connection past its handshake, plus the far end of the
    /// wire it writes to.
    fn welcomed(
        mode: ConnectionMode,
    ) -> (
        Connection<
            tokio::io::ReadHalf<tokio::io::DuplexStream>,
            tokio::io::WriteHalf<tokio::io::DuplexStream>,
        >,
        FrameReader<tokio::io::DuplexStream>,
    ) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (reader, writer) = tokio::io::split(client);
        let conn = Connection::from_halves(
            FrameReader::new(reader),
            FrameWriter::at_build_minor(writer),
            mode,
        );
        (conn, FrameReader::new(server))
    }

    fn rows() -> RegionToDaemonMsg {
        RegionToDaemonMsg::Rows {
            source: RegionSource::Visible,
            ansi: false,
            max_rows: None,
        }
    }

    fn query(len: usize) -> SearchToDaemonMsg {
        SearchToDaemonMsg::Query {
            query: "a".repeat(len),
            options: SearchOptions::default(),
        }
    }

    fn opener(frame: &OwnedFrame) -> (MessageKind, StreamId) {
        let correlation = peek_correlation(&frame.body[..])
            .expect("an opener carries an envelope")
            .expect("an opener carries an envelope");
        let felis_protocol::messages::Correlation::Stream(id) = correlation else {
            panic!("an opener correlates by stream id, got {correlation:?}");
        };
        (MessageKind::from_u16(frame.kind).expect("a known kind"), id)
    }

    /// Consecutive opens on one attached connection take the driver's
    /// ids in order, each stamped on its own opening request.
    #[tokio::test]
    async fn attached_opens_take_the_next_unopened_id_each_time() {
        let (mut conn, mut wire) = welcomed(ConnectionMode::Ops);
        conn.driver.attached();

        let first = conn.open_stream(&rows()).await.unwrap();
        let second = conn.open_stream(&query(6)).await.unwrap();
        let third = conn.open_stream(&rows()).await.unwrap();

        assert_eq!(
            [first.get(), second.get(), third.get()],
            [1, 2, 3],
            "the ids must be the driver's next-unopened sequence"
        );
        for (expected_kind, expected_id) in [
            (MessageKind::Region, first),
            (MessageKind::Search, second),
            (MessageKind::Region, third),
        ] {
            let frame = wire.next_frame().await.unwrap().expect("an opener frame");
            assert_eq!(opener(&frame), (expected_kind, expected_id));
        }
        assert_eq!(
            conn.driver.phase(),
            Phase::Attached,
            "an attached open changes no phase"
        );
    }

    /// The observer's `Setup` → `Observing` transition happens as part
    /// of the open, so the ack cannot arrive at a connection that still
    /// reads as `Setup`.
    #[tokio::test]
    async fn a_notification_subscription_opens_from_setup_and_observes() {
        let (mut conn, mut wire) = welcomed(ConnectionMode::Observer);
        assert_eq!(conn.driver.phase(), Phase::Setup);

        let stream = conn.subscribe_notifications(None).await.unwrap();

        assert_eq!(stream.get(), 1, "the first stream on the connection");
        assert_eq!(conn.driver.phase(), Phase::Observing);
        let frame = wire.next_frame().await.unwrap().expect("an opener frame");
        assert_eq!(opener(&frame), (MessageKind::Notify, stream));
    }

    /// An unsendable body is refused before an id is issued: the id the
    /// daemon expects next is unchanged, so the connection stays usable.
    #[tokio::test]
    async fn a_refused_body_issues_no_stream_id() {
        let (mut conn, mut wire) = welcomed(ConnectionMode::Ops);
        conn.driver.attached();

        let err = conn
            .open_stream(&query(MAX_SEARCH_PATTERN_BYTES + 1))
            .await
            .expect_err("an over-limit pattern is not sendable");
        assert!(matches!(err, ConnectError::Transport(_)), "got {err:?}");

        let stream = conn.open_stream(&query(6)).await.unwrap();
        assert_eq!(stream.get(), 1, "the refused open consumed no id");
        let frame = wire.next_frame().await.unwrap().expect("an opener frame");
        assert_eq!(opener(&frame), (MessageKind::Search, stream));
    }
}

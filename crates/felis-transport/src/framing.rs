//! Async frame reader / writer over any [`AsyncRead`] / [`AsyncWrite`].
//! The length prefix is checked against the ceiling before any body
//! space is reserved (`docs/reference/testing.md` "Security tests", "Frame
//! length ceiling").

use std::io;

use bytes::{Bytes, BytesMut};
use felis_protocol::{
    MessageKind,
    codec::{self, Correlated, WireCodec},
    convert::WireError,
    frame::{DEFAULT_MAX_BODY, Frame, FrameError, HEADER_LEN, LEN_OVERHEAD},
    messages::{Correlation, CorrelationClass, Directed},
    minor::{MinorGated, Requires},
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// Fatal; the connection is torn down on it.
    #[error("frame: {0}")]
    Frame(#[from] FrameError),
    /// A message breached a per-operation limit (REQ-105a) before it
    /// reached the wire. Local and recoverable: nothing was written, so
    /// the connection stays usable.
    #[error("message: {0}")]
    Wire(#[from] WireError),
    #[error("peer closed mid-frame ({pending} bytes pending)")]
    UnexpectedEof { pending: usize },
    /// The message named a minor addition the connection's effective
    /// minor does not define (`docs/reference/ipc.md` "Versioning").
    /// Local and recoverable like [`Self::Wire`]: nothing was written.
    /// A sender that wants to reach this peer downgrades the value or
    /// withholds the message; there is no unchecked way past this.
    #[error("{what} needs protocol minor {needs}, but the connection speaks {effective}")]
    MinorTooOld {
        /// The ledger addition that raised the requirement.
        what: &'static str,
        needs: u16,
        effective: u16,
    },
    /// The envelope the encoder was handed does not match the arm's
    /// [`CorrelationClass`] (`docs/reference/ipc.md` "Correlation,
    /// requests, and streams"); nothing is written. Fields mirror
    /// [`crate::driver::DriverError::Correlation`].
    #[error("correlation violation on {kind}: expected {expected}, found {found}")]
    Correlation {
        kind: MessageKind,
        expected: &'static str,
        found: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedFrame {
    /// Family: see `felis_protocol::MessageKind`.
    pub kind: u16,
    pub body: Bytes,
}

impl OwnedFrame {
    #[cfg(test)]
    #[must_use]
    pub fn as_frame(&self) -> Frame<'_> {
        Frame {
            kind: self.kind,
            body: &self.body,
        }
    }
}

const READ_CHUNK: usize = 8 * 1024;

/// A length prefix is a promise, not proof: reserving the announced body
/// (up to the 64 MiB ceiling) on the header alone would let one header
/// cost more than the frame ever will.
const MAX_READAHEAD: usize = 256 * 1024;

pub struct FrameReader<R> {
    inner: R,
    buf: BytesMut,
    ceiling: u32,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: BytesMut::with_capacity(READ_CHUNK),
            ceiling: DEFAULT_MAX_BODY,
        }
    }

    #[cfg(test)]
    pub fn with_ceiling(inner: R, ceiling: u32) -> Self {
        Self {
            inner,
            buf: BytesMut::with_capacity(READ_CHUNK),
            ceiling,
        }
    }

    /// `Ok(None)` is a clean EOF between frames.
    pub async fn next_frame(&mut self) -> Result<Option<OwnedFrame>, TransportError> {
        loop {
            if let Some(frame) = self.take_frame()? {
                return Ok(Some(frame));
            }
            self.buf.reserve(self.readahead());
            if self.inner.read_buf(&mut self.buf).await? == 0 {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                return Err(TransportError::UnexpectedEof {
                    pending: self.buf.len(),
                });
            }
        }
    }

    /// Resolves when the peer hangs up, without taking a frame.
    ///
    /// Allows callers parked on internal budgets to detect peer disconnection.
    /// Bytes read accumulate in `buf` so cancelling preserves them for the next
    /// [`Self::next_frame`]. Stays pending once `buf` reaches `MAX_READAHEAD`.
    pub async fn wait_for_hangup(&mut self) -> Result<(), TransportError> {
        // The ceiling is the buffer's absolute size, not a relative read-ahead.
        // A per-call baseline would allow writing peers to add another
        // read-ahead on every park, bypassing the admission budget.
        loop {
            if self.buf.len() >= MAX_READAHEAD {
                return std::future::pending().await;
            }
            self.buf.reserve(READ_CHUNK);
            if self.inner.read_buf(&mut self.buf).await? == 0 {
                return Ok(());
            }
        }
    }

    fn take_frame(&mut self) -> Result<Option<OwnedFrame>, TransportError> {
        let Some(total) = self.frame_len()? else {
            return Ok(None);
        };
        if self.buf.len() < total {
            return Ok(None);
        }
        let mut frame = self.buf.split_to(total);
        let kind = u16::from_le_bytes([frame[4], frame[5]]);
        Ok(Some(OwnedFrame {
            kind,
            body: frame.split_off(HEADER_LEN).freeze(),
        }))
    }

    fn frame_len(&self) -> Result<Option<usize>, TransportError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        if len < LEN_OVERHEAD {
            return Err(FrameError::LenUnderflow { len }.into());
        }
        let body_len = len - LEN_OVERHEAD;
        if body_len > self.ceiling {
            return Err(FrameError::BodyTooLarge {
                body_len: u64::from(body_len),
                ceiling: self.ceiling,
            }
            .into());
        }
        Ok(Some(HEADER_LEN + body_len as usize))
    }

    fn readahead(&self) -> usize {
        self.frame_len()
            .ok()
            .flatten()
            .map_or(READ_CHUNK, |total| {
                total.saturating_sub(self.buf.len()).max(READ_CHUNK)
            })
            .min(MAX_READAHEAD)
    }

    /// Discards any read-ahead bytes: a caller swapping carriers does so
    /// before any frame has been read.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

/// Below this a single staged write beats a second syscall on an
/// unbuffered carrier.
const SCRATCH_BYPASS: usize = 8 * 1024;

/// A frame body encoded through the authorization boundary, carrying
/// what it costs to send. Private fields and encoding-only
/// constructors, so a pre-encoded body handed across a queue cannot
/// lose its requirement on the way
/// (`docs/explanation/architecture/ipc.md` "The ledger is the review gate").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedFrame {
    kind: u16,
    body: Vec<u8>,
    requires: Requires,
}

impl CheckedFrame {
    /// # Errors
    /// [`TransportError::Wire`] when the message breaches a
    /// per-operation limit (REQ-105a), and
    /// [`TransportError::Correlation`] when the arm wants an envelope
    /// this path cannot write.
    pub fn encode<M: WireCodec + MinorGated + Directed>(msg: &M) -> Result<Self, TransportError> {
        // Keyed on the arm's class, never on whether the message
        // carries an id: `Conn::Cancel`/`End`/`Error` name a stream in
        // an ordinary field and are `Uncorrelated`, so an
        // id-sniffing gate would refuse the frames the driver requires.
        let meta = msg.meta();
        if meta.correlation != CorrelationClass::Uncorrelated {
            return Err(TransportError::Correlation {
                kind: M::KIND,
                expected: meta.correlation.expects(),
                found: format!("no correlation envelope on {}", meta.name),
            });
        }
        msg.validate()?;
        Ok(Self {
            kind: M::KIND.as_u16(),
            body: codec::encode(msg),
            requires: msg.requires(),
        })
    }

    /// # Errors
    /// [`TransportError::Wire`] as [`Self::encode`], and
    /// [`TransportError::Correlation`] when the arm is uncorrelated or
    /// the id is of the wrong kind.
    pub fn encode_correlated<M: Correlated + MinorGated + Directed>(
        msg: &M,
        correlation: Correlation,
    ) -> Result<Self, TransportError> {
        let meta = msg.meta();
        let paired = matches!(
            (meta.correlation, correlation),
            (
                CorrelationClass::RequestOpener | CorrelationClass::RequestReply,
                Correlation::Request(_)
            ) | (
                CorrelationClass::StreamOpener | CorrelationClass::StreamItem,
                Correlation::Stream(_)
            )
        );
        if !paired {
            return Err(TransportError::Correlation {
                kind: M::KIND,
                expected: meta.correlation.expects(),
                found: format!("{correlation} on {}", meta.name),
            });
        }
        msg.validate()?;
        Ok(Self {
            kind: M::KIND.as_u16(),
            body: codec::encode_correlated(msg, correlation),
            requires: msg.requires(),
        })
    }

    /// A body no domain message produces, for tests that drive the
    /// queue with opaque bytes. Never a production path: production
    /// bodies are authorized by the encoder that made them.
    #[cfg(feature = "test-util")]
    #[must_use]
    pub const fn raw(kind: u16, body: Vec<u8>, requires: Requires) -> Self {
        Self {
            kind,
            body,
            requires,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> u16 {
        self.kind
    }

    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    #[must_use]
    pub const fn requires(&self) -> Requires {
        self.requires
    }
}

pub struct FrameWriter<W> {
    inner: W,
    scratch: Vec<u8>,
    /// The minor both peers speak, fixed at construction.
    effective_minor: u16,
}

impl<W> FrameWriter<W> {
    /// The negotiated minor this writer authorizes sends against
    /// ([`felis_protocol::preface::effective_minor`]). Distinct from the
    /// daemon's own [`felis_protocol::PROTOCOL_MINOR`], which it reports
    /// rather than speaks.
    #[must_use]
    pub const fn effective_minor(&self) -> u16 {
        self.effective_minor
    }

    /// The send-side authorization gate every write passes
    /// (`docs/reference/ipc.md` "Versioning").
    const fn authorize(&self, requires: Requires) -> Result<(), TransportError> {
        if requires.minor > self.effective_minor {
            return Err(TransportError::MinorTooOld {
                what: requires.what,
                needs: requires.minor,
                effective: self.effective_minor,
            });
        }
        Ok(())
    }
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    /// `effective_minor` is what the preface settled on
    /// ([`felis_protocol::preface::effective_minor`]), and it is a
    /// parameter rather than a default because the permissive value is
    /// the one a forgotten call would pick: a writer that assumed this
    /// build's own ceiling would emit additions no peer agreed to.
    pub fn new(inner: W, effective_minor: u16) -> Self {
        Self {
            inner,
            scratch: Vec::with_capacity(READ_CHUNK),
            effective_minor,
        }
    }

    /// A writer for a carrier whose preface has not run: only the base
    /// schema is authorized, since no peer has agreed to more.
    pub fn baseline(inner: W) -> Self {
        Self::new(inner, Requires::BASE.minor)
    }

    /// This build's own [`felis_protocol::PROTOCOL_MINOR`], for a
    /// harness or bench that is both peers and so negotiates nothing.
    /// Never a production path: production writers carry the minor the
    /// preface settled on.
    #[cfg(any(test, feature = "test-util"))]
    pub fn at_build_minor(inner: W) -> Self {
        Self::new(inner, felis_protocol::PROTOCOL_MINOR)
    }

    /// Write a frame without flushing (call [`Self::flush`] if awaiting reply).
    ///
    /// # Errors
    ///
    /// [`TransportError::Frame`] if body exceeds ceiling (refused before write).
    pub(crate) async fn write_frame(&mut self, frame: &Frame<'_>) -> Result<(), TransportError> {
        if frame.body.len() >= SCRATCH_BYPASS {
            let mut header = [0u8; HEADER_LEN];
            let len = LEN_OVERHEAD + frame.checked_body_len()?;
            header[..4].copy_from_slice(&len.to_le_bytes());
            header[4..].copy_from_slice(&frame.kind.to_le_bytes());
            // Two flat writes, not one chained `write_all_buf`: the chain's
            // per-partial-write indirection measured worse than the copy
            // this path avoids.
            self.inner.write_all(&header).await?;
            self.inner.write_all(frame.body).await?;
        } else {
            self.scratch.clear();
            frame.encode_to(&mut self.scratch)?;
            self.inner.write_all(&self.scratch).await?;
        }
        Ok(())
    }

    /// The unchecked write, for tests that need a body no domain
    /// message produces (a bogus kind, an oversized payload). Never a
    /// production path: production writes carry their authorization.
    #[cfg(feature = "test-util")]
    pub async fn write_frame_unchecked(&mut self, frame: &Frame<'_>) -> Result<(), TransportError> {
        self.write_frame(frame).await
    }

    /// `M: Sync` keeps the returned future `Send` (`&M` crosses the
    /// write await; see `clippy::future_not_send`).
    ///
    /// # Errors
    /// [`TransportError::MinorTooOld`] for an unauthorized addition.
    pub async fn send<M: WireCodec + MinorGated + Directed + Sync>(
        &mut self,
        msg: &M,
    ) -> Result<(), TransportError> {
        self.send_unflushed(msg).await?;
        self.flush().await
    }

    pub async fn send_correlated<M: Correlated + MinorGated + Directed + Sync>(
        &mut self,
        msg: &M,
        correlation: Correlation,
    ) -> Result<(), TransportError> {
        let frame = CheckedFrame::encode_correlated(msg, correlation)?;
        self.send_checked(&frame).await
    }

    pub async fn send_unflushed<M: WireCodec + MinorGated + Directed + Sync>(
        &mut self,
        msg: &M,
    ) -> Result<(), TransportError> {
        let frame = CheckedFrame::encode(msg)?;
        self.send_checked_unflushed(&frame).await
    }

    /// Write a body encoded elsewhere. The requirement travels with the
    /// body, so a fan-out that never sees the domain message is
    /// authorized exactly as a direct send is.
    pub async fn send_checked(&mut self, frame: &CheckedFrame) -> Result<(), TransportError> {
        self.send_checked_unflushed(frame).await?;
        self.flush().await
    }

    pub async fn send_checked_unflushed(
        &mut self,
        frame: &CheckedFrame,
    ) -> Result<(), TransportError> {
        self.authorize(frame.requires)?;
        self.write_frame(&Frame {
            kind: frame.kind,
            body: &frame.body,
        })
        .await
    }

    pub async fn flush(&mut self) -> Result<(), TransportError> {
        self.inner.flush().await?;
        Ok(())
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use felis_protocol::frame::{Frame, LEN_OVERHEAD};
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    fn pair() -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
        duplex(64 * 1024)
    }

    #[tokio::test]
    async fn writes_and_reads_a_single_frame() {
        let (a, b) = pair();
        let mut writer = FrameWriter::at_build_minor(a);
        let mut reader = FrameReader::new(b);

        let body = b"hello-felis";
        let frame = Frame { kind: 1, body };
        writer.write_frame(&frame).await.unwrap();
        writer.flush().await.unwrap();

        let got = reader.next_frame().await.unwrap().expect("frame");
        assert_eq!(got.kind, 1);
        assert_eq!(got.body, body[..]);
    }

    /// A header the bypass path mis-encodes surfaces only here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn both_write_paths_produce_the_same_frame() {
        for body_len in [SCRATCH_BYPASS - 1, SCRATCH_BYPASS] {
            let (a, b) = pair();
            let mut writer = FrameWriter::at_build_minor(a);
            let mut reader = FrameReader::new(b);

            let body = vec![0x5Au8; body_len];
            let drain = tokio::spawn(async move { reader.next_frame().await });
            writer
                .write_frame(&Frame {
                    kind: 3,
                    body: &body,
                })
                .await
                .unwrap();
            writer.flush().await.unwrap();

            let got = drain.await.unwrap().unwrap().expect("frame");
            assert_eq!(got.kind, 3);
            assert_eq!(got.body, body[..]);
        }
    }

    /// Both write paths refuse a body past the ceiling, and neither
    /// leaves a byte on the stream for the peer to resynchronize from
    /// (REQ-105). The bypass path is the one that would have written a
    /// truncated header.
    #[tokio::test]
    async fn an_oversized_body_is_refused_before_any_byte_is_written() {
        let body = vec![0u8; DEFAULT_MAX_BODY as usize + 1];
        let (a, mut b) = pair();
        let mut writer = FrameWriter::at_build_minor(a);

        match writer
            .write_frame(&Frame {
                kind: 3,
                body: &body,
            })
            .await
        {
            Err(TransportError::Frame(FrameError::BodyTooLarge { body_len, ceiling })) => {
                assert_eq!(body_len, u64::from(DEFAULT_MAX_BODY) + 1);
                assert_eq!(ceiling, DEFAULT_MAX_BODY);
            }
            other => panic!("expected BodyTooLarge, got {other:?}"),
        }
        writer.flush().await.unwrap();
        drop(writer);

        let mut leaked = Vec::new();
        AsyncReadExt::read_to_end(&mut b, &mut leaked)
            .await
            .unwrap();
        assert!(leaked.is_empty(), "refused frame leaked {leaked:?}");
    }

    /// A body at the ceiling still goes out whole: the refusal is one
    /// byte past, not near.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_body_at_the_ceiling_still_writes() {
        let (a, b) = pair();
        let mut writer = FrameWriter::at_build_minor(a);
        let mut reader = FrameReader::new(b);

        let body = vec![0x5Au8; DEFAULT_MAX_BODY as usize];
        let drain = tokio::spawn(async move { reader.next_frame().await });
        writer
            .write_frame(&Frame {
                kind: 3,
                body: &body,
            })
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let got = drain.await.unwrap().unwrap().expect("frame");
        assert_eq!(got.body.len(), DEFAULT_MAX_BODY as usize);
    }

    /// A semantic limit is refused by `send*` before a body is even
    /// encoded, and the connection survives it: the very next send
    /// lands (REQ-105a).
    #[tokio::test]
    async fn an_over_limit_message_is_refused_without_disturbing_the_stream() {
        use felis_protocol::messages::{
            MAX_SEARCH_PATTERN_BYTES, SearchOptions, SearchToDaemonMsg, StreamId,
        };

        let query = |n: usize| SearchToDaemonMsg::Query {
            query: "a".repeat(n),
            options: SearchOptions::default(),
        };
        let open = |n: u64| Correlation::stream(StreamId::new(n).expect("nonzero"));
        let (a, b) = pair();
        let mut writer = FrameWriter::at_build_minor(a);
        let mut reader = FrameReader::new(b);

        let err = writer
            .send_correlated(&query(MAX_SEARCH_PATTERN_BYTES + 1), open(1))
            .await
            .expect_err("an over-limit search pattern must be refused");
        assert!(matches!(err, TransportError::Wire(_)), "got {err:?}");

        writer
            .send_correlated(&query(MAX_SEARCH_PATTERN_BYTES), open(1))
            .await
            .unwrap();
        let got = reader.next_frame().await.unwrap().expect("frame");
        assert!(!got.body.is_empty());
    }

    /// The whole first-release schema is authorized at minor 0, so the
    /// gate is pinned by the row the first post-release addition will
    /// carry: a body one minor ahead is refused, nothing reaches the
    /// stream, the writer stays usable, and the same body goes out once
    /// the connection defines the minor.
    #[cfg(feature = "test-util")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_future_minor_addition_is_refused_until_the_connection_defines_it() {
        use felis_protocol::{ConnectionMode, PROTOCOL_MINOR, messages::ConnToDaemonMsg};

        let future = Requires::new(PROTOCOL_MINOR + 1, "Future::Arm");
        let frame = || CheckedFrame::raw(0, vec![0x7f], future);
        let hello = ConnToDaemonMsg::Hello {
            mode: ConnectionMode::Ops,
            pull_paced: false,
        };

        let (a, mut b) = pair();
        let mut writer = FrameWriter::new(a, PROTOCOL_MINOR);
        match writer.send_checked(&frame()).await {
            Err(TransportError::MinorTooOld {
                what,
                needs,
                effective,
            }) => assert_eq!(
                (what, needs, effective),
                ("Future::Arm", PROTOCOL_MINOR + 1, PROTOCOL_MINOR)
            ),
            other => panic!("expected MinorTooOld, got {other:?}"),
        }
        writer.send(&hello).await.unwrap();
        drop(writer);

        let mut written = Vec::new();
        AsyncReadExt::read_to_end(&mut b, &mut written)
            .await
            .unwrap();
        assert_eq!(
            written,
            Frame {
                kind: 0,
                body: &codec::encode(&hello),
            }
            .encode()
            .unwrap(),
            "a refused send left bytes on the stream"
        );

        let (a, b) = pair();
        let mut writer = FrameWriter::new(a, PROTOCOL_MINOR + 1);
        let mut reader = FrameReader::new(b);
        writer.send_checked(&frame()).await.unwrap();
        assert_eq!(reader.next_frame().await.unwrap().expect("frame").kind, 0);
    }

    /// A writer built without a negotiated minor authorizes the base
    /// schema and nothing else: the send gate's permissive value is
    /// never what a caller gets by default.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unnegotiated_writer_authorizes_only_the_base_schema() {
        use felis_protocol::messages::ConnToDaemonMsg;

        let (a, b) = pair();
        let mut writer = FrameWriter::baseline(a);
        assert_eq!(writer.effective_minor(), Requires::BASE.minor);

        let mut reader = FrameReader::new(b);
        writer
            .send(&ConnToDaemonMsg::Hello {
                mode: felis_protocol::ConnectionMode::Ops,
                pull_paced: false,
            })
            .await
            .unwrap();
        assert_eq!(reader.next_frame().await.unwrap().expect("frame").kind, 0);
    }

    /// A body encoded away from the writer keeps the requirement its
    /// own metadata reports, so the daemon's pre-encoded fan-out is
    /// gated exactly as a direct send.
    #[tokio::test]
    async fn a_pre_encoded_body_carries_its_own_authorization() {
        use felis_protocol::messages::{AttachFailure, AttachRefusal, SessionToClientMsg};

        let msg = SessionToClientMsg::AttachFailed {
            reason: AttachRefusal::Attach(AttachFailure::SessionExited),
            detail: String::new(),
        };
        let frame = CheckedFrame::encode(&msg).unwrap();
        assert_eq!(frame.requires(), msg.requires());
        assert_eq!(frame.kind(), 4);
        assert_ne!(frame.body(), b"");

        let (a, _b) = pair();
        let mut writer = FrameWriter::new(a, frame.requires().minor);
        writer.send_checked(&frame).await.unwrap();
    }

    /// The uncorrelated path refuses a correlated arm and writes
    /// nothing to the wire.
    #[tokio::test]
    async fn a_correlated_arm_is_refused_by_the_uncorrelated_send_path() {
        use felis_protocol::messages::NotifyToDaemonMsg;

        let (a, mut b) = pair();
        let mut writer = FrameWriter::at_build_minor(a);
        match writer
            .send(&NotifyToDaemonMsg::Subscribe {
                session_prefix: None,
            })
            .await
        {
            Err(TransportError::Correlation {
                kind,
                expected,
                found,
            }) => {
                assert_eq!(kind, MessageKind::Notify);
                assert_eq!(expected, CorrelationClass::StreamOpener.expects());
                assert_eq!(found, "no correlation envelope on Notify::Subscribe");
            }
            other => panic!("expected a correlation refusal, got {other:?}"),
        }
        drop(writer);

        let mut leaked = Vec::new();
        AsyncReadExt::read_to_end(&mut b, &mut leaked)
            .await
            .unwrap();
        assert!(leaked.is_empty(), "refused frame leaked {leaked:?}");
    }

    /// An arm that wants no envelope cannot be handed one, and an id of
    /// the wrong kind is refused in both directions (a stream on a
    /// request class and a request on a stream class).
    #[tokio::test]
    async fn a_mismatched_envelope_is_refused_by_the_correlated_send_path() {
        use felis_protocol::messages::{
            NotifyToDaemonMsg, OpsToDaemonMsg, RequestId, SessionToDaemonMsg, StreamId,
        };

        let (a, mut b) = pair();
        let mut writer = FrameWriter::at_build_minor(a);

        let stream = Correlation::stream(StreamId::new(1).expect("nonzero"));
        let request = Correlation::request(RequestId::new(1).expect("nonzero"));
        for (found, class, err) in [
            (
                "stream 1 on Session::Detach",
                CorrelationClass::Uncorrelated,
                writer
                    .send_correlated(&SessionToDaemonMsg::Detach, stream)
                    .await,
            ),
            (
                "stream 1 on Ops::Status",
                CorrelationClass::RequestOpener,
                writer
                    .send_correlated(&OpsToDaemonMsg::Status, stream)
                    .await,
            ),
            (
                "request 1 on Notify::Subscribe",
                CorrelationClass::StreamOpener,
                writer
                    .send_correlated(
                        &NotifyToDaemonMsg::Subscribe {
                            session_prefix: None,
                        },
                        request,
                    )
                    .await,
            ),
        ] {
            match err {
                Err(TransportError::Correlation {
                    expected,
                    found: got,
                    ..
                }) => {
                    assert_eq!((got.as_str(), expected), (found, class.expects()));
                }
                other => panic!("expected a correlation refusal for {found}, got {other:?}"),
            }
        }
        drop(writer);

        let mut leaked = Vec::new();
        AsyncReadExt::read_to_end(&mut b, &mut leaked)
            .await
            .unwrap();
        assert!(leaked.is_empty(), "refused frame leaked {leaked:?}");
    }

    /// The honest pairings still go out.
    #[tokio::test]
    async fn each_class_goes_out_through_the_path_that_matches_it() {
        use felis_protocol::messages::{
            NotifyToDaemonMsg, OpsToDaemonMsg, RequestId, SessionToDaemonMsg, StreamId,
        };

        let (a, b) = pair();
        let mut writer = FrameWriter::at_build_minor(a);
        let mut reader = FrameReader::new(b);

        writer.send(&SessionToDaemonMsg::Detach).await.unwrap();
        writer
            .send_correlated(
                &NotifyToDaemonMsg::Subscribe {
                    session_prefix: None,
                },
                Correlation::stream(StreamId::new(1).expect("nonzero")),
            )
            .await
            .unwrap();
        writer
            .send_correlated(
                &OpsToDaemonMsg::Status,
                Correlation::request(RequestId::new(1).expect("nonzero")),
            )
            .await
            .unwrap();

        for kind in [4u16, 7, 5] {
            let got = reader.next_frame().await.unwrap().expect("frame");
            assert_eq!(got.kind, kind);
        }
    }

    /// `Conn::Cancel` names a stream in an ordinary field and is
    /// `Uncorrelated`, and still sends without an envelope.
    #[tokio::test]
    async fn an_uncorrelated_arm_naming_a_stream_inline_still_sends() {
        use felis_protocol::messages::{ConnToDaemonMsg, StreamId};

        let (a, b) = pair();
        let mut writer = FrameWriter::at_build_minor(a);
        let mut reader = FrameReader::new(b);
        writer
            .send(&ConnToDaemonMsg::Cancel {
                stream_id: StreamId::new(1).expect("nonzero"),
            })
            .await
            .unwrap();
        let got = reader.next_frame().await.unwrap().expect("frame");
        assert_eq!(got.kind, 0);
    }

    #[tokio::test]
    async fn reassembles_frames_split_across_chunks() {
        let (mut a, b) = pair();
        let mut reader = FrameReader::new(b);

        let body = b"reassemble-me";
        let frame = Frame { kind: 0, body };
        let bytes = frame.encode().unwrap();
        for chunk in bytes.chunks(3) {
            a.write_all(chunk).await.unwrap();
            a.flush().await.unwrap();
        }
        let got = reader.next_frame().await.unwrap().expect("frame");
        assert_eq!(got.body, body[..]);
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let (a, b) = pair();
        drop(a);
        let mut reader = FrameReader::new(b);
        let got = reader.next_frame().await.unwrap();
        assert!(got.is_none(), "expected None, got {got:?}");
    }

    #[tokio::test]
    async fn mid_frame_eof_is_unexpected() {
        let (mut a, b) = pair();
        let mut reader = FrameReader::new(b);

        let len: u32 = LEN_OVERHEAD + 100;
        a.write_all(&len.to_le_bytes()).await.unwrap();
        a.write_all(&[0]).await.unwrap();
        drop(a);

        match reader.next_frame().await {
            Err(TransportError::UnexpectedEof { pending }) => {
                assert_eq!(pending, 5);
            }
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn body_above_ceiling_is_a_fatal_frame_error() {
        let (mut a, b) = pair();
        let mut reader = FrameReader::with_ceiling(b, 32);

        let len: u32 = LEN_OVERHEAD + 200;
        a.write_all(&len.to_le_bytes()).await.unwrap();
        a.flush().await.unwrap();

        match reader.next_frame().await {
            Err(TransportError::Frame(FrameError::BodyTooLarge { body_len, ceiling })) => {
                assert_eq!(body_len, 200);
                assert_eq!(ceiling, 32);
            }
            other => panic!("expected BodyTooLarge, got {other:?}"),
        }
    }

    /// The buffer grows with the bytes that arrive, never with the
    /// announced size.
    #[tokio::test]
    async fn a_large_announcement_does_not_reserve_its_whole_promise() {
        let (mut a, b) = pair();
        let mut reader = FrameReader::new(b);

        let len: u32 = LEN_OVERHEAD + 60 * 1024 * 1024;
        a.write_all(&len.to_le_bytes()).await.unwrap();
        a.write_all(&[0u8]).await.unwrap();
        a.flush().await.unwrap();

        let poll =
            tokio::time::timeout(std::time::Duration::from_millis(50), reader.next_frame()).await;
        assert!(poll.is_err(), "an incomplete frame must keep waiting");
        assert!(
            reader.buf.capacity() <= MAX_READAHEAD * 2,
            "reserved {} bytes for a 60 MiB announcement",
            reader.buf.capacity()
        );
    }

    #[tokio::test]
    async fn one_read_can_serve_several_frames() {
        let (mut a, b) = pair();
        let mut reader = FrameReader::new(b);

        let mut wire = Vec::new();
        for i in 0..8u16 {
            wire.extend_from_slice(
                &Frame {
                    kind: i,
                    body: b"row",
                }
                .encode()
                .unwrap(),
            );
        }
        a.write_all(&wire).await.unwrap();
        a.flush().await.unwrap();

        for i in 0..8u16 {
            let got = reader.next_frame().await.unwrap().expect("frame");
            assert_eq!(got.kind, i);
            assert_eq!(got.body, b"row"[..]);
        }
    }

    /// The ceiling check fires per frame, not cumulatively over a burst.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn image_burst_streams_through_within_ceiling() {
        use felis_protocol::{
            ImageId, MessageKind,
            codec::encode,
            messages::{ImageFormat, ImageMsg, ImageTarget, MAX_IMAGE_CHUNK_PAYLOAD},
        };

        let (a, b) = pair();
        let mut writer = FrameWriter::at_build_minor(a);
        let mut reader = FrameReader::new(b);

        let chunk_size = MAX_IMAGE_CHUNK_PAYLOAD;
        let header = ImageMsg::Header {
            id: ImageId(1),
            target: ImageTarget::New {
                width: 1024,
                height: 768,
                format: ImageFormat::Rgba32,
            },
        };
        let chunks: Vec<ImageMsg> = (0..3)
            .map(|i| ImageMsg::Chunk {
                id: ImageId(1),
                bytes: vec![i as u8; chunk_size].into(),
            })
            .collect();
        let complete = ImageMsg::Complete { id: ImageId(1) };

        let messages = std::iter::once(header)
            .chain(chunks.iter().cloned())
            .chain(std::iter::once(complete))
            .collect::<Vec<_>>();
        let frame_count = messages.len();

        // Each chunk exceeds the duplex's 64 KiB capacity, so the reader
        // must run concurrently or the writer blocks.
        let drain = tokio::spawn(async move {
            for _ in 0..frame_count {
                let got = reader
                    .next_frame()
                    .await
                    .unwrap()
                    .expect("frame from burst");
                assert_eq!(got.kind, MessageKind::Image.as_u16());
            }
        });

        for msg in &messages {
            let body = encode(msg);
            writer
                .write_frame(&Frame {
                    kind: MessageKind::Image.as_u16(),
                    body: &body,
                })
                .await
                .unwrap();
        }
        writer.flush().await.unwrap();
        drop(writer);
        drain.await.unwrap();
    }

    #[tokio::test]
    async fn round_trips_many_frames_in_sequence() {
        let (a, b) = pair();
        let mut writer = FrameWriter::at_build_minor(a);
        let mut reader = FrameReader::new(b);

        let frames: Vec<_> = (0..16)
            .map(|i| OwnedFrame {
                kind: (i % 3) as u16,
                body: Bytes::from(format!("body-{i}")),
            })
            .collect();
        for f in &frames {
            writer.write_frame(&f.as_frame()).await.unwrap();
        }
        writer.flush().await.unwrap();
        for expected in &frames {
            let got = reader.next_frame().await.unwrap().expect("frame");
            assert_eq!(&got, expected);
        }
    }

    #[tokio::test]
    async fn a_hangup_watch_stays_pending_while_the_peer_is_only_quiet() {
        let (a, b) = pair();
        let mut writer = FrameWriter::at_build_minor(a);
        let mut reader = FrameReader::new(b);
        writer
            .write_frame(&Frame {
                kind: 7,
                body: b"queued",
            })
            .await
            .unwrap();
        writer.flush().await.unwrap();

        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                reader.wait_for_hangup(),
            )
            .await
            .is_err(),
            "a peer that is holding its socket open has not hung up",
        );

        drop(writer);
        tokio::time::timeout(std::time::Duration::from_secs(5), reader.wait_for_hangup())
            .await
            .expect("the hangup must be noticed")
            .unwrap();

        // The bytes the watch read ahead of the frame boundary are still
        // the reader's; a caller that abandoned the race can read on.
        let got = reader.next_frame().await.unwrap().expect("frame");
        assert_eq!((got.kind, &got.body[..]), (7, b"queued".as_slice()));
    }

    /// The caller parks once per message it cannot admit, so the watch
    /// runs many times over one connection while the peer keeps writing.
    /// The read-ahead it admits is a total, not a per-call allowance.
    #[tokio::test]
    async fn repeated_hangup_watches_share_one_read_ahead_ceiling() {
        let (mut a, b) = pair();
        let mut reader = FrameReader::new(b);

        // Larger than the ceiling and written from a task, so the writer
        // keeps offering bytes for every watch below instead of the
        // socket buffer emptying between them.
        let feeder = tokio::spawn(async move {
            let chunk = vec![0u8; 64 * 1024];
            loop {
                if a.write_all(&chunk).await.is_err() {
                    return;
                }
                if a.flush().await.is_err() {
                    return;
                }
            }
        });

        for watch in 0..8 {
            drop(
                tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    reader.wait_for_hangup(),
                )
                .await,
            );
            // A read in flight when the ceiling is reached may overshoot
            // it by the spare capacity it was given; what must not happen
            // is the watch count multiplying the ceiling.
            assert!(
                reader.buf.len() <= MAX_READAHEAD * 2,
                "watch {watch} left {} buffered bytes",
                reader.buf.len(),
            );
        }
        feeder.abort();
    }
}

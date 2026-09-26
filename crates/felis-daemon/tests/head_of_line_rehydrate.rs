//! REQ-103 / REQ-1011: a stalled rehydration burst cannot block input.
//!
//! Uses [`StallAfter`] in `poll_write` to deterministically park the daemon's
//! writer mid-burst before asserting input delivery.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use felis_daemon::pool::SessionPool;
use felis_daemon::serve::{DaemonCaps, SessionFactory, handle_stream};
use felis_protocol::{
    ConnectionMode, MessageKind, codec,
    messages::{
        AttachTarget, ConnToDaemonMsg, ImageMsg, InputMsg, SessionToClientMsg, SessionToDaemonMsg,
        SpawnArgs,
    },
    preface::ClientPreface,
};
use felis_pty::Command;
use felis_transport::framing::{FrameReader, FrameWriter};
use tokio::io::{AsyncWrite, DuplexStream};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Notify;

mod common;
use common::b64;

/// 1024 × 2048 × 3 = 6 MiB of raw RGB: the rehydrate burst is three
/// orders of magnitude past [`STALL_AFTER`], so the gate closes deep
/// inside it.
const IMAGE_W: usize = 1024;
const IMAGE_H: usize = 2048;

const WIRE_BUF: usize = 64 * 1024;

/// Large enough to carry the handshake and the session ack, and
/// under [`WIRE_BUF`] so the gate, not a full duplex buffer, is what
/// stalls the writer.
const STALL_AFTER: usize = 32 * 1024;

/// Per REQ-1011; newline-terminated so the fixture child's `read`
/// returns one line per input.
const INPUT_LEN: usize = 200;

const INPUTS: usize = 24;

/// After `STALL_AFTER` bytes, `poll_write` returns `Pending` without
/// arming a waker: what a peer that stopped reading looks like to the
/// daemon.
struct StallAfter {
    inner: DuplexStream,
    written: Arc<AtomicUsize>,
    stalled: Arc<Notify>,
}

impl StallAfter {
    fn gate(&self) -> Option<usize> {
        let done = self.written.load(Ordering::SeqCst);
        if done >= STALL_AFTER {
            self.stalled.notify_one();
            return None;
        }
        Some(STALL_AFTER - done)
    }
}

impl AsyncWrite for StallAfter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let Some(budget) = self.gate() else {
            return Poll::Pending;
        };
        let take = buf.len().min(budget);
        let inner = Pin::new(&mut self.inner);
        match inner.poll_write(cx, &buf[..take]) {
            Poll::Ready(Ok(n)) => {
                self.written.fetch_add(n, Ordering::SeqCst);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.gate().is_none() {
            return Poll::Pending;
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

struct Conn {
    reader: FrameReader<DuplexStream>,
    writer: FrameWriter<DuplexStream>,
}

impl Conn {
    async fn open<W>(
        pool: &Arc<AsyncMutex<SessionPool>>,
        factory: &SessionFactory,
        mode: ConnectionMode,
        wrap: impl FnOnce(DuplexStream) -> W,
    ) -> Self
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (client_to_daemon, daemon_read) = tokio::io::duplex(WIRE_BUF);
        let (daemon_write, daemon_to_client) = tokio::io::duplex(WIRE_BUF);
        let daemon_write = wrap(daemon_write);
        let server_pool = Arc::clone(pool);
        let server_factory = Arc::clone(factory);
        drop(tokio::spawn(async move {
            handle_stream(
                daemon_read,
                daemon_write,
                DaemonCaps::default(),
                server_pool,
                server_factory,
            )
            .await
        }));

        // Preface before framing: a `FrameReader` built first would
        // absorb the daemon's preface reply.
        let (mut daemon_to_client, mut client_to_daemon) = (daemon_to_client, client_to_daemon);
        felis_transport::preface::write_client_preface(
            &mut client_to_daemon,
            ClientPreface::CURRENT,
        )
        .await
        .unwrap();
        felis_transport::preface::read_daemon_preface(&mut daemon_to_client)
            .await
            .unwrap();

        let mut conn = Self {
            reader: FrameReader::new(daemon_to_client),
            writer: FrameWriter::at_build_minor(client_to_daemon),
        };
        conn.writer
            .send(&ConnToDaemonMsg::Hello {
                mode,
                pull_paced: false,
            })
            .await
            .unwrap();
        conn.reader.next_frame().await.unwrap().expect("welcome");
        conn
    }

    /// Either ack: a create attaches, so both arms mean "subscribed".
    async fn wait_ready(&mut self) -> u128 {
        loop {
            let frame = self.reader.next_frame().await.unwrap().expect("ready");
            if frame.kind != MessageKind::Session.as_u16() {
                continue;
            }
            match codec::decode::<SessionToClientMsg>(&frame.body).unwrap() {
                SessionToClientMsg::Attached { info } | SessionToClientMsg::Created { info } => {
                    return info.id;
                }
                _ => {}
            }
        }
    }
}

/// The runner's `PATH` is appended rather than dropped: NixOS links
/// only `sh` into `/bin`, so the child would resolve no `cat`,
/// transmit no image, and the test would wait out its whole budget.
fn fixture_path() -> std::ffi::OsString {
    let mut path = std::ffi::OsString::from("/bin:/usr/bin");
    if let Some(host) = std::env::var_os("PATH") {
        path.push(":");
        path.push(host);
    }
    path
}

/// The marker file is the observation point: the stalled connection's
/// own wire can never show that its input arrived.
fn fixture_child(apc: &Path, marker: &Path) -> Command {
    let script = format!(
        "IFS= read -r _go; cat '{}'; while IFS= read -r line; do printf 'x\\n' >> '{}'; done",
        apc.display(),
        marker.display(),
    );
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", &script]);
    cmd.env_clear();
    cmd.env("PATH", fixture_path());
    cmd.env("TERM", "xterm-256color");
    cmd
}

fn marker_lines(marker: &Path) -> usize {
    std::fs::read_to_string(marker).map_or(0, |s| s.lines().count())
}

/// Input arriving while a rehydration burst is stalled mid-write must
/// still reach the PTY (REQ-103, REQ-1011).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_reaches_the_pty_while_the_rehydrate_burst_is_stalled_mid_write() {
    let dir = tempfile::tempdir().unwrap();
    let pixels = dir.path().join("pixels.rgb");
    std::fs::write(&pixels, vec![0x40u8; IMAGE_W * IMAGE_H * 3]).unwrap();
    let apc = dir.path().join("transmit.esc");
    std::fs::write(
        &apc,
        [
            b"\x1b_Ga=t,i=1,f=24,t=f,".to_vec(),
            format!("s={IMAGE_W},v={IMAGE_H};").into_bytes(),
            b64(pixels.to_str().unwrap().as_bytes()),
            b"\x1b\\".to_vec(),
        ]
        .concat(),
    )
    .unwrap();
    let marker = dir.path().join("input.log");

    let pool = Arc::new(AsyncMutex::new(SessionPool::new()));
    let child = Mutex::new(Some(fixture_child(&apc, &marker)));
    let factory: SessionFactory = Arc::new(move |_| {
        child
            .lock()
            .unwrap()
            .take()
            .expect("only the first connection creates a session")
    });

    // Windows, because only a window's burst carries the image store.
    let mut a = Conn::open(&pool, &factory, ConnectionMode::Window, |w| w).await;
    a.writer
        .send(&SessionToDaemonMsg::Create {
            args: SpawnArgs {
                env: vec![("TERM".to_owned(), "xterm-256color".to_owned())],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let id = a.wait_ready().await;

    a.writer
        .send(&InputMsg::KeyBytes(b"go\n".to_vec()))
        .await
        .unwrap();
    loop {
        let frame = a.reader.next_frame().await.unwrap().expect("image stream");
        if frame.kind == MessageKind::Image.as_u16()
            && matches!(
                codec::decode::<ImageMsg>(&frame.body).unwrap(),
                ImageMsg::Complete { .. }
            )
        {
            break;
        }
    }
    // A keeps draining so the session task never sees both
    // subscribers backed up.
    let drain_a =
        tokio::spawn(
            async move { while a.reader.next_frame().await.is_ok_and(|f| f.is_some()) {} },
        );

    let written = Arc::new(AtomicUsize::new(0));
    let stalled = Arc::new(Notify::new());
    let mut b = {
        let written = Arc::clone(&written);
        let stalled = Arc::clone(&stalled);
        Conn::open(&pool, &factory, ConnectionMode::Window, move |inner| {
            StallAfter {
                inner,
                written,
                stalled,
            }
        })
        .await
    };
    b.writer
        .send(&SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(id),
            live_only: false,
        })
        .await
        .unwrap();

    // The gate fires from inside the daemon's own `poll_write`, so
    // this returning is the daemon being stalled mid-burst.
    tokio::time::timeout(Duration::from_secs(20), stalled.notified())
        .await
        .expect("the daemon's writer never reached the stall gate");
    let stalled_at = written.load(Ordering::SeqCst);
    assert_eq!(
        stalled_at, STALL_AFTER,
        "the gate must close on the full budget, not on a short write"
    );

    let line = {
        let mut line = vec![b'a'; INPUT_LEN - 1];
        line.push(b'\n');
        line
    };
    for _ in 0..INPUTS {
        b.writer
            .send_unflushed(&InputMsg::KeyBytes(line.clone()))
            .await
            .unwrap();
    }
    b.writer.flush().await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let seen = marker_lines(&marker);
        if seen >= INPUTS {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "only {seen} of {INPUTS} inputs reached the child: the stalled rehydrate \
             burst head-of-line blocked the input sharing its connection",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        written.load(Ordering::SeqCst),
        STALL_AFTER,
        "the burst must still be undrained: every input above overtook it on a \
         connection whose outbound direction never moved"
    );

    drain_a.abort();
}

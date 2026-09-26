//! `felis-daemon relay` through a real `ssh` hop against a test-owned
//! sshd (docs/reference/ipc.md "Cross-host carrier: SSH stdio").
//! Skips when the OpenSSH tooling is missing; the dev shell provides
//! it. `cross_host_stdio.rs` covers the same wire without the hop.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![expect(
    clippy::print_stderr,
    reason = "skip diagnostics are intentional, mirroring real_app_harness"
)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use felis_client_core::ShadowScreen;

mod common;
use common::{daemon_bin, resolve_binary, shadow_contains, wait_connectable};
use felis_protocol::{
    ConnectionMode, MessageKind, codec,
    messages::{
        ConnToClientMsg, ConnToDaemonMsg, GridMsg, InputMsg, SessionToClientMsg,
        SessionToDaemonMsg, SpawnArgs,
    },
    preface::{ClientPreface, DaemonPreface},
};
use felis_transport::framing::{FrameReader, FrameWriter};
use felis_transport::{StdioSession, spawn_command};
use tokio::io::{AsyncRead, AsyncWrite};

struct SshFixture {
    dir: tempfile::TempDir,
    port: u16,
    user: String,
    ssh: PathBuf,
    identity: PathBuf,
    known_hosts: PathBuf,
    _sshd: tokio::process::Child,
}

impl SshFixture {
    async fn spawn() -> Option<Self> {
        let Some(sshd_bin) = resolve_binary("sshd") else {
            eprintln!("skipping: sshd not on PATH (enter the dev shell for openssh)");
            return None;
        };
        let Some(ssh) = resolve_binary("ssh") else {
            eprintln!("skipping: ssh not on PATH");
            return None;
        };
        let Some(keygen) = resolve_binary("ssh-keygen") else {
            eprintln!("skipping: ssh-keygen not on PATH");
            return None;
        };
        // A non-root sshd can only auth its own uid.
        let Ok(user) = std::env::var("USER") else {
            eprintln!("skipping: USER is unset, cannot derive the ssh login name");
            return None;
        };
        let dir = tempfile::tempdir().expect("create sshd fixture dir");
        // REQ-107: the daemon socket this fixture holds needs a 0700
        // parent this uid owns, and `tempdir` follows the umask.
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("tighten the fixture directory");
        let host_key = dir.path().join("host_ed25519");
        let identity = dir.path().join("client_ed25519");
        run_keygen(&keygen, &host_key).await;
        run_keygen(&keygen, &identity).await;

        let client_pub = std::fs::read_to_string(identity.with_extension("pub")).unwrap();
        let authorized_keys = dir.path().join("authorized_keys");
        std::fs::write(&authorized_keys, &client_pub).unwrap();

        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();

        // StrictModes off: the tempdir's ancestors (a sticky /tmp) fail
        // sshd's ownership walk. UsePAM off: a non-root sshd cannot run
        // PAM session hooks.
        let config_path = dir.path().join("sshd_config");
        let config = format!(
            "Port {port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey {host_key}\n\
             PidFile none\n\
             AuthorizedKeysFile {auth}\n\
             StrictModes no\n\
             UsePAM no\n\
             PasswordAuthentication no\n\
             KbdInteractiveAuthentication no\n\
             LogLevel VERBOSE\n",
            host_key = host_key.display(),
            auth = authorized_keys.display(),
        );
        std::fs::write(&config_path, config).unwrap();

        let host_pub = std::fs::read_to_string(host_key.with_extension("pub")).unwrap();
        let known_hosts = dir.path().join("known_hosts");
        std::fs::write(&known_hosts, format!("[127.0.0.1]:{port} {host_pub}")).unwrap();

        // Without -D sshd daemonizes and kill_on_drop reaps the wrong
        // process.
        let mut cmd = tokio::process::Command::new(&sshd_bin);
        cmd.arg("-D").arg("-e").arg("-f").arg(&config_path);
        cmd.kill_on_drop(true);
        let mut sshd = cmd.spawn().expect("spawn sshd");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = sshd.try_wait().unwrap() {
                panic!("sshd exited during startup: {status}");
            }
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "sshd did not accept on 127.0.0.1:{port} within 10 s"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let fixture = Self {
            dir,
            port,
            user,
            ssh,
            identity,
            known_hosts,
            _sshd: sshd,
        };

        // sshd runs every remote command through the login shell from
        // the user database (not `$SHELL`, and no sshd_config knob
        // overrides it), so a `nologin` CI account answers every exec
        // with a banner. Probing the real path rather than looking the
        // shell up also covers hosts without `getent`.
        let canary = fixture
            .ssh_command("echo felis-canary")
            .stdin(Stdio::null())
            .output()
            .await
            .expect("spawn the canary ssh exec");
        if !canary.stdout.starts_with(b"felis-canary") {
            eprintln!(
                "skipping: the sshd hop cannot exec commands as {} \
                 (login shell unusable?): stdout={:?} stderr={:?}",
                fixture.user,
                String::from_utf8_lossy(&canary.stdout),
                String::from_utf8_lossy(&canary.stderr),
            );
            return None;
        }

        Some(fixture)
    }

    /// `-F /dev/null` keeps the user's `~/.ssh/config` out.
    fn ssh_command(&self, remote_command: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.ssh);
        cmd.arg("-F").arg("/dev/null");
        for opt in [
            "BatchMode=yes".to_owned(),
            "IdentitiesOnly=yes".to_owned(),
            "StrictHostKeyChecking=yes".to_owned(),
            "GlobalKnownHostsFile=/dev/null".to_owned(),
            "ConnectTimeout=5".to_owned(),
            format!("IdentityFile={}", self.identity.display()),
            format!("UserKnownHostsFile={}", self.known_hosts.display()),
        ] {
            cmd.arg("-o").arg(opt);
        }
        cmd.arg("-p").arg(self.port.to_string());
        cmd.arg(format!("{}@127.0.0.1", self.user));
        cmd.arg(remote_command);
        cmd
    }

    async fn spawn_persistent_daemon(&self) -> (PathBuf, tokio::process::Child) {
        let socket = self.dir.path().join("daemon.sock");
        let child = tokio::process::Command::new(daemon_bin())
            .arg("serve")
            .arg("--socket")
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn persistent daemon");
        wait_connectable(&socket).await;
        (socket, child)
    }
}

async fn run_keygen(keygen: &Path, key: &Path) {
    let status = tokio::process::Command::new(keygen)
        .args(["-q", "-t", "ed25519", "-N", ""])
        .arg("-f")
        .arg(key)
        .status()
        .await
        .expect("spawn ssh-keygen");
    assert!(status.success(), "ssh-keygen failed for {}", key.display());
}

/// Runs on the raw child pipes: a `FrameReader` built first would read
/// ahead and swallow the daemon's preface reply.
async fn exchange_preface<R, W>(read_half: &mut R, write_half: &mut W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use felis_transport::preface::{read_daemon_preface, write_client_preface};

    write_client_preface(write_half, ClientPreface::CURRENT)
        .await
        .unwrap();
    let reply = read_daemon_preface(read_half).await.unwrap();
    assert!(
        matches!(reply, DaemonPreface::Accept { .. }),
        "the relay must carry the preface through to an accepting daemon, got {reply:?}"
    );
}

async fn next_control<R, M>(reader: &mut FrameReader<R>, kind: MessageKind, what: &str) -> M
where
    R: AsyncRead + Unpin,
    M: codec::WireCodec,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let frame = tokio::time::timeout_at(deadline, reader.next_frame())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what} through ssh"))
            .unwrap()
            .unwrap_or_else(|| panic!("stream closed waiting for {what}"));
        if frame.kind == kind.as_u16() {
            return codec::decode(&frame.body).unwrap();
        }
    }
}

async fn handshake<R, W>(reader: &mut FrameReader<R>, writer: &mut FrameWriter<W>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    writer
        .send(&ConnToDaemonMsg::Hello {
            mode: ConnectionMode::Window,
            pull_paced: false,
        })
        .await
        .unwrap();
    match next_control::<_, ConnToClientMsg>(reader, MessageKind::Conn, "Welcome").await {
        ConnToClientMsg::Welcome { .. } => {}
        other => panic!("expected Welcome, got {other:?}"),
    }
}

async fn prefaced(
    session: StdioSession,
) -> (
    FrameReader<tokio::process::ChildStdout>,
    FrameWriter<tokio::process::ChildStdin>,
) {
    let StdioSession { reader, writer, .. } = session;
    let mut read_half = reader.into_inner();
    let mut write_half = writer.into_inner();
    exchange_preface(&mut read_half, &mut write_half).await;
    (
        FrameReader::new(read_half),
        FrameWriter::at_build_minor(write_half),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_to_felis_daemon_relay_handshakes() {
    let Some(fx) = SshFixture::spawn().await else {
        return;
    };
    let (socket, _daemon) = fx.spawn_persistent_daemon().await;
    // Single quotes survive whichever login shell parses the remote
    // command (POSIX sh and fish alike).
    let (mut reader, mut writer) = prefaced(
        spawn_command(fx.ssh_command(&format!(
            "'{}' relay --socket '{}'",
            daemon_bin(),
            socket.display()
        )))
        .expect("spawn ssh felis-daemon relay"),
    )
    .await;

    handshake(&mut reader, &mut writer).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_session_echoes_keystrokes_back_through_the_hop() {
    let Some(fx) = SshFixture::spawn().await else {
        return;
    };
    // Absolute path: the daemon inherits sshd's minimal environment,
    // whose PATH need not contain coreutils (NixOS, notably).
    let Some(cat) = resolve_binary("cat") else {
        eprintln!("skipping: cat not on PATH");
        return;
    };
    let (socket, _daemon) = fx.spawn_persistent_daemon().await;
    let (mut reader, mut writer) = prefaced(
        spawn_command(fx.ssh_command(&format!(
            "'{}' relay --socket '{}'",
            daemon_bin(),
            socket.display()
        )))
        .expect("spawn ssh felis-daemon relay"),
    )
    .await;

    handshake(&mut reader, &mut writer).await;

    let create = SessionToDaemonMsg::Create {
        args: SpawnArgs {
            command: cat.display().to_string(),
            ..SpawnArgs::default()
        },
    };
    writer.send(&create).await.unwrap();
    let (rows, cols) = match next_control::<_, SessionToClientMsg>(
        &mut reader,
        MessageKind::Session,
        "SessionCreated",
    )
    .await
    {
        SessionToClientMsg::Created { info } => (info.dims.rows, info.dims.cols),
        other => panic!("expected SessionToClientMsg::Created, got {other:?}"),
    };

    let marker = "felis-over-ssh";
    writer
        .send(&InputMsg::KeyBytes(format!("{marker}\r").into_bytes()))
        .await
        .unwrap();

    let mut shadow = ShadowScreen::new(rows, cols);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let frame = tokio::time::timeout_at(deadline, reader.next_frame())
            .await
            .expect("marker should render within 20 s through ssh")
            .unwrap()
            .expect("stream closed before the marker rendered");
        if frame.kind != MessageKind::Grid.as_u16() {
            continue;
        }
        let msg: GridMsg = codec::decode(&frame.body).unwrap();
        drop(shadow.apply(&msg));
        if shadow_contains(&shadow, marker) {
            break;
        }
    }
}

/// A dropped SSH link tears down only the relay; the persistent daemon
/// and its sessions keep running (docs/reference/ipc.md "Cross-host
/// carrier: SSH stdio").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_link_drop_preserves_the_persistent_daemon() {
    let Some(fx) = SshFixture::spawn().await else {
        return;
    };
    let (socket, _daemon) = fx.spawn_persistent_daemon().await;

    let remote = format!("'{}' relay --socket '{}'", daemon_bin(), socket.display());
    let mut cmd = fx.ssh_command(&remote);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().expect("spawn ssh felis-daemon stdio relay");
    let mut write_half = child.stdin.take().unwrap();
    let mut read_half = child.stdout.take().unwrap();
    exchange_preface(&mut read_half, &mut write_half).await;
    let mut writer = FrameWriter::at_build_minor(write_half);
    let mut reader = FrameReader::new(read_half);

    handshake(&mut reader, &mut writer).await;

    child.kill().await.unwrap();
    drop(writer);
    drop(reader);

    let survived = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(
        survived.is_ok(),
        "persistent daemon stopped answering after the SSH link dropped"
    );
}

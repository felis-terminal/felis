"""Tests for share/felis/shell-integration/ssh.sh and its shell wrappers.

Hermetic: stub `ssh` and `infocmp` executables on PATH record each call,
so the tests pin which ssh runs happen, with which arguments, TERM and
stdin, without a network or an sshd.
"""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
INTEGRATION = ROOT / "share" / "felis" / "shell-integration"
SCRIPT = INTEGRATION / "ssh.sh"
SEP = "\x1f"

DEFAULT_CONFIG = """\
user bob
hostname vm.example
port 22
requesttty auto
sessiontype default
stdinnull no
forkafterauthentication no
tunnel false
controlmaster false
permitlocalcommand no
clearallforwardings no
"""

STUB_SSH = r"""#!/bin/sh
log=$STUB_DIR/calls
args=
for arg; do args=$args$arg$(printf '\037'); done
case "$1 $args" in
"-G "*RemoteCommand=none*)
	printf 'probe-config\t%s\t%s\n' "$TERM" "$args" >>"$log"
	cat "$STUB_DIR/probe-config"
	;;
"-G "*)
	printf 'login-config\t%s\t%s\n' "$TERM" "$args" >>"$log"
	cat "$STUB_DIR/login-config"
	;;
*RemoteCommand=none*)
	printf 'probe\t%s\t%s\n' "$TERM" "$args" >>"$log"
	cat >"$STUB_DIR/probe-stdin"
	exit "${STUB_PROBE_STATUS:-0}"
	;;
*)
	printf 'session\t%s\t%s\n' "$TERM" "$args" >>"$log"
	exit "${STUB_SESSION_STATUS:-0}"
	;;
esac
"""

STUB_INFOCMP = r"""#!/bin/sh
[ -z "$STUB_INFOCMP_FAIL" ] || exit 1
printf 'xterm-felis|felis terminal emulator,\n\tam,\n'
"""


class Run:
    def __init__(self, completed: subprocess.CompletedProcess[str], stub: Path):
        self.status = completed.returncode
        self.stderr = completed.stderr
        log = stub / "calls"
        self.calls = []
        for line in log.read_text().splitlines() if log.exists() else []:
            kind, term, args = line.split("\t", 2)
            self.calls.append((kind, term, args.split(SEP)[:-1]))
        stdin = stub / "probe-stdin"
        self.probe_stdin = stdin.read_text() if stdin.exists() else None

    def kinds(self) -> list[str]:
        return [kind for kind, _, _ in self.calls]

    def session(self) -> tuple[str, list[str]]:
        sessions = [
            (term, args) for kind, term, args in self.calls if kind == "session"
        ]
        assert len(sessions) == 1, (self.calls, self.stderr)
        return sessions[0]


class Stub:
    def __init__(self, path: Path):
        self.path = path
        for name, body in (("ssh", STUB_SSH), ("infocmp", STUB_INFOCMP)):
            script = path / name
            script.write_text(body)
            script.chmod(0o755)
        self.configure(DEFAULT_CONFIG)

    def configure(self, login: str, probe: str | None = None) -> None:
        (self.path / "login-config").write_text(login)
        (self.path / "probe-config").write_text(login if probe is None else probe)

    def run_command(
        self, argv: list[str], term: str = "xterm-felis", **env: str
    ) -> Run:
        completed = subprocess.run(
            argv,
            env={
                **os.environ,
                "PATH": f"{self.path}{os.pathsep}{os.environ['PATH']}",
                "STUB_DIR": str(self.path),
                "TERM": term,
                **env,
            },
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            check=False,
        )
        return Run(completed, self.path)

    def ssh(self, *args: str, term: str = "xterm-felis", **env: str) -> Run:
        return self.run_command(["sh", str(SCRIPT), *args], term=term, **env)


@pytest.fixture
def stub(tmp_path: Path) -> Stub:
    return Stub(tmp_path)


def assert_bypassed(run: Run, args: list[str]) -> None:
    assert "probe" not in run.kinds()
    assert [(term, call) for _, term, call in run.calls][-1] == ("xterm-felis", args)
    assert run.stderr == ""


def probe_args(run: Run) -> list[str]:
    return next(args for kind, _, args in run.calls if kind == "probe")


def test_outside_felis_runs_ssh_unchanged(stub: Stub) -> None:
    run = stub.ssh("vm", term="xterm-256color")
    assert run.kinds() == ["session"]
    assert run.session() == ("xterm-256color", ["vm"])


def test_interactive_login_installs_the_entry_then_logs_in(stub: Stub) -> None:
    run = stub.ssh("-p", "2222", "vm")
    assert run.kinds() == ["login-config", "probe-config", "probe", "session"]
    assert run.session() == ("xterm-felis", ["-p", "2222", "vm"])
    assert run.probe_stdin == "xterm-felis|felis terminal emulator,\n\tam,\n"
    args = probe_args(run)
    assert args[:7] == [
        "-T",
        "-o",
        "RequestTTY=no",
        "-o",
        "RemoteCommand=none",
        "-o",
        "ClearAllForwardings=yes",
    ]
    assert args[7:10] == ["-p", "2222", "vm"]
    assert args[10].startswith("sh -c '")
    assert run.status == 0
    assert run.stderr == ""


@pytest.mark.parametrize(
    "args",
    [
        ["-p2222", "vm"],
        ["vm", "-p", "2222"],
        ["-A", "-o", "Port=1", "vm"],
        ["--", "vm"],
    ],
)
def test_options_in_every_ssh_position_still_count_as_a_login(
    stub: Stub, args: list[str]
) -> None:
    assert "probe" in stub.ssh(*args).kinds()


@pytest.mark.parametrize(
    "args",
    [["vm", "uptime"], ["-l", "bob", "vm", "ls", "-l"], ["--", "vm", "ls"]],
)
def test_a_remote_command_bypasses(stub: Stub, args: list[str]) -> None:
    assert_bypassed(stub.ssh(*args), args)


@pytest.mark.parametrize(
    "args",
    [
        ["-N", "vm"],
        ["-tt", "vm"],
        ["-T", "vm"],
        ["-f", "vm"],
        ["-W", "h:22", "vm"],
        ["-Snone", "vm"],
        ["-M", "vm"],
        ["-w", "0", "vm"],
        ["-O", "check", "vm"],
        ["-G", "vm"],
        ["-V"],
        [],
    ],
)
def test_non_login_options_bypass(stub: Stub, args: list[str]) -> None:
    assert_bypassed(stub.ssh(*args), args)


@pytest.mark.parametrize(
    "line",
    [
        "remotecommand tmux attach",
        "requesttty force",
        "sessiontype none",
        "stdinnull yes",
        "forkafterauthentication yes",
        "tunnel true",
        "controlmaster true",
        "controlmaster ask",
        "permitlocalcommand yes",
        "setenv TERM=xterm-256color",
    ],
)
def test_effective_configuration_that_conflicts_bypasses(stub: Stub, line: str) -> None:
    key = line.split()[0]
    kept = [entry for entry in DEFAULT_CONFIG.splitlines() if entry.split()[0] != key]
    stub.configure("\n".join([*kept, line]) + "\n")
    assert_bypassed(stub.ssh("vm"), ["vm"])


def test_a_shared_multiplexing_master_does_not_bypass(stub: Stub) -> None:
    stub.configure(DEFAULT_CONFIG.replace("controlmaster false", "controlmaster auto"))
    assert "probe" in stub.ssh("vm").kinds()


def test_a_probe_routed_elsewhere_by_match_bypasses(stub: Stub) -> None:
    stub.configure(DEFAULT_CONFIG, DEFAULT_CONFIG.replace("port 22", "port 2222"))
    assert_bypassed(stub.ssh("vm"), ["vm"])


def test_probe_only_keys_do_not_count_as_a_different_route(stub: Stub) -> None:
    probe = DEFAULT_CONFIG.replace("requesttty auto", "requesttty false").replace(
        "clearallforwardings no", "clearallforwardings yes"
    )
    stub.configure(DEFAULT_CONFIG + "localforward 8080 [localhost]:80\n", probe)
    assert "probe" in stub.ssh("-L", "8080:localhost:80", "vm").kinds()


def test_a_failed_install_warns_and_falls_back(stub: Stub) -> None:
    run = stub.ssh("vm", STUB_PROBE_STATUS="4")
    assert run.session() == ("xterm-256color", ["vm"])
    assert (
        run.stderr
        == "felis: xterm-felis is not available on vm; using TERM=xterm-256color\n"
    )


def test_a_missing_local_entry_warns_without_probing(stub: Stub) -> None:
    run = stub.ssh("vm", STUB_INFOCMP_FAIL="1")
    assert "probe" not in run.kinds()
    assert run.session() == ("xterm-256color", ["vm"])
    assert "using TERM=xterm-256color" in run.stderr


def test_an_ssh_failure_on_the_probe_ends_without_a_second_connection(
    stub: Stub,
) -> None:
    run = stub.ssh("vm", STUB_PROBE_STATUS="255")
    assert "session" not in run.kinds()
    assert run.status == 255
    assert run.stderr == ""


def test_the_session_exit_status_is_returned(stub: Stub) -> None:
    assert stub.ssh("vm", STUB_SESSION_STATUS="7").status == 7


def test_the_remote_script_survives_non_posix_login_shells(stub: Stub) -> None:
    script = probe_args(stub.ssh("vm"))[-1]
    assert "\n" not in script
    assert "!" not in script
    assert "\\" not in script
    assert "'" not in script.removeprefix("sh -c '").removesuffix("'")


# A startup file such as NixOS's /etc/zshenv can rebuild PATH, so the stub
# directory is put first again after the shell has started.
@pytest.mark.parametrize(
    ("shell", "source"),
    [
        ("bash", 'PATH={stub}:$PATH; source {dir}/ssh.bash; ssh "$@"'),
        ("zsh", 'PATH={stub}:$PATH; source {dir}/ssh.zsh; ssh "$@"'),
        ("fish", "set -gx PATH {stub} $PATH; source {dir}/ssh.fish; ssh $argv"),
    ],
)
def test_each_shell_wrapper_forwards_arguments_intact(
    stub: Stub, shell: str, source: str
) -> None:
    assert shutil.which(shell) is not None, (
        f"{shell} is not on PATH; the dev shell provides it"
    )
    args = ["vm", "echo", "two words", "*"]
    command = source.format(stub=stub.path, dir=INTEGRATION)
    argv = [shell, "-c", command, *(["--"] if shell == "fish" else ["ssh"]), *args]
    assert stub.run_command(argv).session() == ("xterm-felis", args)


STUB_TIC = r"""#!/bin/sh
[ -z "$STUB_TIC_FAIL" ] || exit 1
[ "$1 $2 $3" = "-x -o $HOME/.terminfo" ] || exit 2
mkdir -p "$3/x" && cat >"$3/x/xterm-felis"
"""

STUB_REMOTE_INFOCMP = r"""#!/bin/sh
[ -z "$STUB_LOOKUP_FAIL" ] || exit 1
[ -e "$HOME/.terminfo/x/xterm-felis" ] || [ -n "$STUB_SYSTEM_ENTRY" ]
"""


class Remote:
    """Runs the script the probe sends against stub tic and infocmp in a throwaway HOME."""

    def __init__(self, root: Path, script: str):
        self.home = root / "home"
        self.home.mkdir()
        self.bin = root / "bin"
        self.bin.mkdir()
        for tool in ("sh", "mkdir", "cat"):
            found = shutil.which(tool)
            assert found is not None, tool
            (self.bin / tool).symlink_to(found)
        self.script = script

    def stub(self, *names: str) -> None:
        bodies = {"tic": STUB_TIC, "infocmp": STUB_REMOTE_INFOCMP}
        for name in names:
            path = self.bin / name
            path.write_text(bodies[name])
            path.chmod(0o755)

    def run(self, **env: str) -> int:
        return subprocess.run(
            [str(self.bin / "sh"), "-c", self.script],
            env={"PATH": str(self.bin), "HOME": str(self.home), **env},
            input="xterm-felis|felis terminal emulator,\n",
            capture_output=True,
            text=True,
            check=False,
        ).returncode

    def installed(self) -> bool:
        return (self.home / ".terminfo" / "x" / "xterm-felis").exists()


@pytest.fixture
def remote(tmp_path: Path) -> Remote:
    stub_dir = tmp_path / "stub"
    stub_dir.mkdir()
    script = probe_args(Stub(stub_dir).ssh("vm"))[-1]
    remote_dir = tmp_path / "remote"
    remote_dir.mkdir()
    return Remote(remote_dir, script)


def test_tic_compiles_the_entry_into_the_home_terminfo(remote: Remote) -> None:
    remote.stub("tic", "infocmp")
    assert remote.run() == 0
    assert remote.installed()


def test_an_existing_entry_is_recompiled(remote: Remote) -> None:
    remote.stub("tic", "infocmp")
    assert remote.run(STUB_SYSTEM_ENTRY="1") == 0
    assert remote.installed()


def test_a_failed_compile_is_reported(remote: Remote) -> None:
    remote.stub("tic", "infocmp")
    assert remote.run(STUB_TIC_FAIL="1") == 4


def test_an_entry_that_does_not_resolve_after_compiling_is_reported(
    remote: Remote,
) -> None:
    remote.stub("tic", "infocmp")
    assert remote.run(STUB_LOOKUP_FAIL="1") == 5


def test_without_infocmp_a_successful_compile_counts(remote: Remote) -> None:
    remote.stub("tic")
    assert remote.run() == 0
    assert remote.installed()


def test_without_tic_an_existing_entry_counts(remote: Remote) -> None:
    remote.stub("infocmp")
    assert remote.run(STUB_SYSTEM_ENTRY="1") == 0


def test_without_tic_or_an_entry_the_install_fails(remote: Remote) -> None:
    remote.stub("infocmp")
    assert remote.run() == 3
    assert not remote.installed()

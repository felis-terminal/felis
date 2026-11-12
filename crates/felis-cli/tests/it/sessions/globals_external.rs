use super::*;

/// The stderr assertion pins the guard: exit 2 also spells
/// "unreachable", which every argv here would produce with the check
/// deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_global_carrier_on_a_retarget_is_a_usage_error() {
    for argv in [
        &["--socket", "/tmp/felis-absent.sock", "window", "retarget"][..],
        &["--host", "elsewhere", "ssh", "vm"],
    ] {
        let out = cli_command()
            .args(argv)
            .output()
            .expect("run the retarget verb");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{argv:?} must exit 2: stderr={stderr}"
        );
        assert!(
            stderr.contains("--host/--socket/--ssh-arg")
                && stderr.contains("addressed to the window's own daemon"),
            "{argv:?} must be refused by the global-carrier guard, not by a failed dial: \
             stderr={stderr}",
        );
    }
}

/// Refused before any lookup, so the command need not exist, and
/// `--config` is refused before it is resolved: a path that does not
/// exist still fails naming the command, never as a missing file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_global_before_an_external_command_is_a_usage_error() {
    for (argv, global) in [
        (
            &["--socket", "/tmp/felis-absent.sock", "absent"][..],
            "--socket",
        ),
        (&["--host", "elsewhere", "absent"], "--host"),
        (
            &["--host", "elsewhere", "--ssh-arg=-p", "absent"],
            "--host/--ssh-arg",
        ),
        (
            &["--config", "/nonexistent/work.toml", "absent"],
            "--config",
        ),
    ] {
        let out = cli_command()
            .args(argv)
            .output()
            .expect("run the external command");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{argv:?} must exit 2: stderr={stderr}"
        );
        assert!(
            stderr.contains(&format!("({global})")) && stderr.contains("felis-absent"),
            "{argv:?} must be refused by name: stderr={stderr}",
        );
    }
}

/// The two grammar-printing verbs dial nothing, so a carrier global is
/// refused before any output reaches stdout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_global_carrier_on_a_grammar_verb_is_a_usage_error() {
    let tmp = private_dir();
    let man = tmp.path().join("man");
    let man = man.to_str().expect("a utf-8 temp path");
    for (argv, verb) in [
        (
            &["--socket", "/tmp/felis-absent.sock", "completions", "fish"][..],
            "felis completions",
        ),
        (
            &["--host", "elsewhere", "completions", "fish"],
            "felis completions",
        ),
        (
            &["--host", "elsewhere", "--ssh-arg=-p", "__mangen", man],
            "felis __mangen",
        ),
        (
            &["--socket", "/tmp/felis-absent.sock", "__mangen", man],
            "felis __mangen",
        ),
    ] {
        let out = cli_command().args(argv).output().expect("run the verb");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{argv:?} must exit 2: stderr={stderr}"
        );
        assert!(
            out.stdout.is_empty(),
            "{argv:?} must print nothing: {out:?}"
        );
        assert!(
            stderr.contains("--host/--socket/--ssh-arg") && stderr.contains(verb),
            "{argv:?} must be refused by name: stderr={stderr}",
        );
    }
}

/// A `PATH` holding only `dir`, so the lookup sees the fixtures and
/// nothing ambient.
fn external_command(dir: &std::path::Path) -> StdCommand {
    let mut cmd = cli_command();
    cmd.env("PATH", dir);
    cmd
}

fn place_script(dir: &std::path::Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write the script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make the script executable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_external_command_receives_the_remaining_arguments_verbatim() {
    let tmp = private_dir();
    place_script(
        tmp.path(),
        "felis-echo",
        "#!/bin/sh\nprintf '%s\\n' \"$@\"\n",
    );
    let out = external_command(tmp.path())
        .args(["echo", "a b", "--format", "json", "--trace-perf"])
        .output()
        .expect("run the external command");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "a b\n--format\njson\n--trace-perf\n"
    );

    let out = external_command(tmp.path())
        .args(["help", "echo", "sub"])
        .output()
        .expect("run help for the external command");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "sub\n--help\n");

    let out = external_command(tmp.path())
        .args(["--trace-perf", "echo"])
        .output()
        .expect("run the external command");
    assert_eq!(
        out.status.code(),
        Some(2),
        "root --trace-perf must be an unknown argument: {out:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_external_command_is_a_usage_error_naming_the_similar_verb() {
    let tmp = private_dir();
    let out = external_command(tmp.path())
        .args(["session", "list"])
        .output()
        .expect("run the misspelled verb");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr={stderr}");
    assert!(
        stderr.contains("no `felis-session` beside felis or on PATH")
            && stderr.contains("a similar subcommand exists: 'sessions'"),
        "stderr={stderr}",
    );
}

/// An execute bit this process holds no right to (here: others only,
/// on a file the test owns) is `EACCES` at exec time, and the lookup
/// moves on to the next directory as `execvp` would.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_external_command_this_process_may_not_run_yields_to_a_later_one() {
    use std::os::unix::fs::PermissionsExt;
    let denied = private_dir();
    let allowed = private_dir();
    place_script(denied.path(), "felis-echo", "#!/bin/sh\necho denied\n");
    std::fs::set_permissions(
        denied.path().join("felis-echo"),
        std::fs::Permissions::from_mode(0o001),
    )
    .expect("leave only the others' execute bit");
    place_script(allowed.path(), "felis-echo", "#!/bin/sh\necho allowed\n");
    let path = std::env::join_paths([denied.path(), allowed.path()]).expect("join PATH");
    let out = cli_command()
        .env("PATH", path)
        .arg("echo")
        .output()
        .expect("run the external command");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "allowed\n");
}

/// `exec` of a script whose interpreter is missing fails with `ENOENT`
/// too; the lookup already found the command, so this is a launch
/// failure, not an unknown verb.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_external_command_that_fails_to_launch_is_not_an_unknown_verb() {
    let tmp = private_dir();
    place_script(tmp.path(), "felis-broken", "#!/nonexistent/interpreter\n");
    let out = external_command(tmp.path())
        .arg("broken")
        .output()
        .expect("run the broken command");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr={stderr}");
    assert!(
        stderr.contains(&tmp.path().join("felis-broken").display().to_string())
            && !stderr.contains("unrecognized subcommand"),
        "stderr={stderr}",
    );
}

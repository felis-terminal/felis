//! Shell completions generator and dynamic session completion helper.
//!
//! Emits completion scripts with overlays for dynamic session listing.

use clap::CommandFactory;
use clap_complete::{Shell, generate};
use felis_protocol::SessionHex;

use crate::Cli;
use felis_client_core::{Carrier, Reconnector, RemoteSpawn};

/// The `sessions` subcommands whose first positional is a session id,
/// in one place so the fish loop and the zsh overlay comment cannot
/// list different sets.
pub(crate) const SESSION_ID_VERBS: &[&str] = &[
    "info", "send", "kill", "evict", "capture", "search", "switch", "tag",
];

pub(crate) fn emit(shell: Shell) {
    let script = script(shell);
    #[allow(clippy::print_stdout)]
    {
        print!("{script}");
    }
}

/// The `clap_complete` script plus, for fish and zsh, the overlay.
fn script(shell: Shell) -> String {
    let mut cmd = Cli::command();
    let mut buf = Vec::new();
    generate(shell, &mut cmd, "felis", &mut buf);
    let mut script = String::from_utf8(buf).unwrap_or_default();

    let overlay: &str = match shell {
        Shell::Fish => &fish_overlay(),
        Shell::Zsh => ZSH_OVERLAY,
        // Bash's completion descriptions land on a second screen line,
        // so the title-and-cwd detail would be invisible at the prompt.
        _ => "",
    };

    if !overlay.is_empty() {
        script.push('\n');
        script.push_str(overlay);
        script.push('\n');
    }
    script
}

/// Completion runs on every `<TAB>`, which is no place to start a
/// daemon; the SSH carrier is dropped before this even applies (#42).
pub(crate) const REMOTE_SPAWN: RemoteSpawn = RemoteSpawn::Refuse;

/// Lists the local daemon's roster; an SSH carrier yields nothing
/// without spawning `ssh` (see the module doc). Always exits 0.
pub(crate) fn run_complete_sessions(
    runtime: &tokio::runtime::Runtime,
    target: &Reconnector,
) -> i32 {
    if matches!(target.carrier, Carrier::Ssh { .. }) {
        return 0;
    }
    let sessions = runtime.block_on(async move {
        match crate::conn::dial(target, REMOTE_SPAWN).await {
            Ok(mut conn) => conn.list_sessions().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    });

    for s in &sessions {
        let line = format_completion_line(s);
        #[allow(clippy::print_stdout)]
        {
            println!("{line}");
        }
    }
    0
}

/// One `<id-hex>\t<description>` line: the title, the cwd, or
/// `"<title> — <cwd>"` (the separator `sessions list` uses). Control
/// characters are stripped: a `\n` breaks fish's `\t`-delimited
/// candidate parser, and a tab merges with the id column.
#[must_use]
pub(crate) fn format_completion_line(s: &felis_protocol::messages::SessionInfo) -> String {
    let title = s
        .title
        .as_deref()
        .map(|t| sanitize_completion_field(t.trim()))
        .unwrap_or_default();
    let cwd = s
        .cwd
        .as_deref()
        .map(|c| sanitize_completion_field(c.trim()))
        .unwrap_or_default();
    let desc = match (title.is_empty(), cwd.is_empty()) {
        (true, true) => String::new(),
        (false, true) => title,
        (true, false) => cwd,
        (false, false) => format!("{title} \u{2014} {cwd}"),
    };
    format!("{}\t{desc}", SessionHex(s.id))
}

fn sanitize_completion_field(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// fish merges completion directives, so the overlay adds to
/// `clap_complete`'s output rather than overriding it.
fn fish_overlay() -> String {
    format!(
        r#"# felis: dynamic session-id completion overlay.
#
# Calls the hidden `felis __complete-sessions` helper, which
# emits one `<id>\t<description>` line per live session. fish
# parses the description column itself; the id ends up as the
# candidate, the title/cwd as the popup detail.

# Completion is local-only: the helper never dials SSH. An in-flight
# --host means the verb will talk to a remote daemon, so the local
# roster would be the wrong candidates; the helpers then emit nothing
# rather than start an `ssh` that could sit on a password prompt
# under a <TAB>.

function __felis_carrier_flags
    # Echo the in-flight --socket tokens, one per line; fail on --host
    # so the caller emits nothing. Only the global carrier counts: a
    # retarget verb's own destination names the daemon the window is
    # moving *to*, while these slots name a session on the daemon this
    # command already speaks to, so the scan stops at that verb. The
    # global flags are not clap globals, so they precede it.
    set -l tokens (commandline -opc)
    set -l i 1
    while test $i -le (count $tokens)
        switch $tokens[$i]
            case --host '--host=*'
                return 1
            case --socket
                echo -- $tokens[$i]
                set i (math $i + 1)
                test $i -le (count $tokens); and echo -- $tokens[$i]
            case '--socket=*'
                echo -- $tokens[$i]
            case --ssh-arg
                # Skipped with its value: an ssh token spelled like a
                # verb would otherwise end the scan before the --host
                # it requires.
                set i (math $i + 1)
            case '--ssh-arg=*'
            case '*'
                contains -- $tokens[$i] retarget ssh; and break
        end
        set i (math $i + 1)
    end
    return 0
end

function __felis_complete_sessions
    set -l carrier (__felis_carrier_flags); or return
    felis $carrier __complete-sessions 2>/dev/null
end

function __felis_complete_retarget_target_sessions
    # `--session <TAB>` on a retarget names a session on the *target*
    # daemon: the socket `window retarget` names is dialed in place of
    # the current carrier; an SSH destination yields no candidates.
    set -l tokens (commandline -opc)
    set -l carrier
    set -l past_verb 0
    set -l i 1
    while test $i -le (count $tokens)
        set -l t $tokens[$i]
        if test $past_verb -eq 0
            switch $t
                case --host '--host=*' ssh
                    return
            end
            contains -- $t retarget; and set past_verb 1
        else
            switch $t
                case --session --from --attachment --format
                    set i (math $i + 1)
                case '--*'
                case '*'
                    set carrier --socket $t
            end
        end
        set i (math $i + 1)
    end
    felis $carrier __complete-sessions 2>/dev/null
end

function __felis_ssh_destination_slot
    # True only in `felis ssh`'s own destination slot: after the verb,
    # before any `--` separator, and while the positional is still
    # empty. An option's value is stepped over so it cannot read as the
    # destination, and a filled slot falls through to fish's own file
    # completion rather than to a host list.
    set -l tokens (commandline -opc)
    set -l seen 0
    set -l i 1
    while test $i -le (count $tokens)
        set -l t $tokens[$i]
        if test $seen -eq 0
            switch $t
                case --config --socket
                    set i (math $i + 1)
                case --host --ssh-arg
                    set i (math $i + 1)
                case ssh
                    set seen 1
                case '*'
            end
        else
            switch $t
                case --
                    return 1
                case --ssh-arg --session --from --attachment --format
                    set i (math $i + 1)
                case '--*'
                case '*'
                    return 1
            end
        end
        set i (math $i + 1)
    end
    test $seen -eq 1
end

# `felis sessions <verb> <TAB>` — the id is the first positional after
# the subcommand, for every session-id-taking verb.
for __felis_sub in {verbs}
    complete -c felis -x -a '(__felis_complete_sessions)' \
        -n "__fish_seen_subcommand_from sessions; and __fish_seen_subcommand_from $__felis_sub"
end
set -e __felis_sub

# `felis attach <TAB>` — a top-level verb, not a `sessions`
# subcommand, so it gets its own line.
complete -c felis -x -a '(__felis_complete_sessions)' \
    -n "__fish_seen_subcommand_from attach"

# Option-valued session-id slots. `switch --from` and `notifications
# subscribe --session` name sessions on the dialed daemon; a retarget's
# two slots split — `--from` is the local window's session, `--session`
# lives on the *target* daemon.
complete -c felis -x -l from -a '(__felis_complete_sessions)' \
    -n "__fish_seen_subcommand_from sessions; and __fish_seen_subcommand_from switch"
complete -c felis -x -l session -a '(__felis_complete_sessions)' \
    -n "__fish_seen_subcommand_from notifications; and __fish_seen_subcommand_from subscribe"
complete -c felis -x -l from -a '(__felis_complete_sessions)' \
    -n "__fish_seen_subcommand_from retarget ssh"
complete -c felis -x -l session -a '(__felis_complete_retarget_target_sessions)' \
    -n "__fish_seen_subcommand_from retarget ssh"

# Both SSH destination slots — the global `--host` and `felis ssh`'s
# own positional — borrow fish's own host completer (known_hosts + ssh
# config), the source its `ssh` completions read. felis never parses a
# destination, so the candidate set is whatever `ssh` accepts.
complete -c felis -x -l host -a '(__fish_complete_user_at_hosts)'
complete -c felis -x -a '(__fish_complete_user_at_hosts)' \
    -n "__felis_ssh_destination_slot"
"#,
        verbs = SESSION_ID_VERBS.join(" ")
    )
}

/// Post-processes generated zsh completion to swap session-id slots for the dynamic helper.
///
/// `zsh_base_script_contains_the_value_name_anchors` guards the expected value-name anchors.
const ZSH_OVERLAY: &str = r#"# felis: dynamic session-id completion overlay.
#
# `_felis_complete_sessions` runs the hidden helper and turns
# each `<id>\t<description>` line into a `_describe` candidate so
# the title/cwd shows up next to the id in zsh's completion menu.

# Completion is local-only: the helpers never dial SSH. An in-flight
# --host means the verb will talk to a remote daemon, so the local
# roster would be the wrong candidates; the helpers then emit nothing
# rather than start an `ssh` that could sit on a password prompt
# under a <TAB>.

_felis_complete_sessions() {
    local -a sessions carrier
    local id desc i
    # Forward the in-flight --socket value; bail on --host. Only the
    # global carrier counts: a retarget verb's own destination names
    # the daemon the window is moving *to*, while these slots name a
    # session on the daemon this command already speaks to, so the scan
    # stops at that verb.
    for (( i = 2; i <= $#words; i++ )); do
        case $words[i] in
            --host|--host=*)
                return 0 ;;
            --socket)
                carrier+=($words[i])
                if (( i < $#words )); then
                    (( i++ ))
                    carrier+=($words[i])
                fi
                ;;
            --socket=*)
                carrier+=($words[i])
                ;;
            --ssh-arg)
                # Skipped with its value: an ssh token spelled like a
                # verb would otherwise end the scan before the --host
                # it requires.
                (( i++ )) ;;
            --ssh-arg=*)
                ;;
            retarget|ssh)
                break ;;
        esac
    done
    # Command substitution, not `< <(...)` and not a pipe: process
    # substitution needs `/dev/fd`, which a locked-down completion
    # environment need not provide, and whether a pipeline's last stage
    # shares this function's scope is not something every zsh build
    # agrees on — a subshell there would swallow the appends and leave
    # `sessions` empty.
    local line
    for line in ${(f)"$(felis $carrier __complete-sessions 2>/dev/null)"}; do
        [[ -n $line ]] || continue
        id=${line%%$'\t'*}
        desc=${line#*$'\t'}
        [[ $desc == $line ]] && desc=
        if [[ -n $desc ]]; then
            sessions+=("${id}:${desc}")
        else
            sessions+=("${id}")
        fi
    done
    _describe -t sessions 'session' sessions
}

_felis_complete_retarget_target_sessions() {
    # `--session` on a retarget names a session on the *target* daemon:
    # the socket `window retarget` names is dialed in place of the
    # current carrier; an SSH destination yields no candidates.
    local -a sessions carrier
    local id desc i past_verb=0
    for (( i = 2; i <= $#words; i++ )); do
        [[ $words[i] == (--host|--host=*) ]] && return 0
        if (( ! past_verb )); then
            [[ $words[i] == ssh ]] && return 0
            [[ $words[i] == retarget ]] && past_verb=1
            continue
        fi
        case $words[i] in
            --session|--from|--attachment|--format)
                (( i++ )) ;;
            --*)
                ;;
            *)
                carrier=(--socket $words[i]) ;;
        esac
    done
    local line
    for line in ${(f)"$(felis $carrier __complete-sessions 2>/dev/null)"}; do
        [[ -n $line ]] || continue
        id=${line%%$'\t'*}
        desc=${line#*$'\t'}
        [[ $desc == $line ]] && desc=
        if [[ -n $desc ]]; then
            sessions+=("${id}:${desc}")
        else
            sessions+=("${id}")
        fi
    done
    _describe -t sessions 'session' sessions
}

# Post-process the clap_complete-generated `_felis` function to
# route session-id slots through `_felis_complete_sessions`. The
# anonymous function scopes `extendedglob` (needed for `(#b)` /
# `[^:]##`) so we don't change global shell options.
() {
    emulate -L zsh
    setopt extendedglob
    local def=${functions[_felis]}
    # The `:id -- <help text>:_default` positional on top-level `attach`
    # and every session-id-taking `sessions <verb>` (felis's
    # SESSION_ID_VERBS) — one pattern catches them all (each id slot uses
    # the same `_default` value action, so the verbs need no per-verb
    # enumeration here). Preserve the help text so zsh still labels the
    # slot in the completion popup.
    def=${def//(#b)(:id -- [^:]##):_default/${match[1]}:_felis_complete_sessions}
    # The option-valued session-id slots, anchored on their clap value
    # names: ID-OR-PREFIX (`sessions switch --from`, `notifications
    # subscribe --session`) and PREFIX (a retarget's `--from`) name
    # sessions on the dialed daemon; TARGET-PREFIX (a retarget's
    # `--session`) lives on the retarget target and gets the
    # target-aware helper.
    def=${def//(#b)(:ID-OR-PREFIX):_default/${match[1]}:_felis_complete_sessions}
    def=${def//(#b)(:TARGET-PREFIX):_default/${match[1]}:_felis_complete_retarget_target_sessions}
    def=${def//(#b)(:PREFIX):_default/${match[1]}:_felis_complete_sessions}
    # Every SSH destination — the global `--host` and `felis ssh`'s own
    # positional — routes through zsh's `_hosts` completer
    # (known_hosts + ssh config), the same source zsh's own `ssh`
    # completion uses. felis never parses a destination, so the
    # candidate set is whatever `ssh` accepts. Both flags share the
    # `user@host` value name, which is quote- and colon-free, so one
    # exact-match substitution covers them.
    def=${def//(#b)(:user@host):_default/${match[1]}:_hosts}
    functions[_felis]=$def
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use felis_protocol::messages::SessionInfo;

    fn s(id: u128, title: Option<&str>, cwd: Option<&str>) -> SessionInfo {
        SessionInfo {
            id,
            dims: felis_protocol::messages::GridDims {
                rows: 24,
                cols: 80,
                pixel_w: 0,
                pixel_h: 0,
            },
            title: title.map(str::to_owned),
            cwd: cwd.map(str::to_owned),
            idle_seconds: None,
            tags: Vec::new(),
            last_notification: None,
            foreground: None,
            exited: false,
            last_exit_code: None,
            attachments: Vec::new(),
            sequence: std::num::NonZeroU64::MIN,
        }
    }

    proptest::proptest! {
        /// fish parses the line itself: the id column ends at the first
        /// tab and the description must survive as one candidate, so no
        /// control character may reach it. Which of the title and the cwd
        /// the description carries follows from which of them has any
        /// content left after stripping.
        #[test]
        fn a_completion_line_is_an_id_column_and_a_control_free_description(
            title in proptest::option::of(".*"),
            cwd in proptest::option::of(".*"),
        ) {
            let line = format_completion_line(&s(0xab, title.as_deref(), cwd.as_deref()));
            let (id, desc) = line.split_once('\t').expect("the id column ends at a tab");
            proptest::prop_assert_eq!(id, "000000000000000000000000000000ab");
            proptest::prop_assert!(!desc.chars().any(char::is_control));

            let shown = |field: Option<&String>| -> String {
                field.map_or_else(String::new, |f| {
                    f.trim().chars().filter(|c| !c.is_control()).collect()
                })
            };
            let (title, cwd) = (shown(title.as_ref()), shown(cwd.as_ref()));
            let separator = usize::from(!title.is_empty() && !cwd.is_empty()) * " \u{2014} ".len();
            proptest::prop_assert!(desc.starts_with(&title) && desc.ends_with(&cwd));
            proptest::prop_assert_eq!(desc.len(), title.len() + separator + cwd.len());
        }
    }

    #[test]
    fn fish_overlay_wires_both_destination_spellings_to_the_host_completer() {
        // Losing either line drops that destination slot back to no
        // completion.
        let overlay = fish_overlay();
        assert!(overlay.contains("-l host -a '(__fish_complete_user_at_hosts)'"));
        assert!(overlay.contains(
            "-a '(__fish_complete_user_at_hosts)' \\\n    -n \"__felis_ssh_destination_slot\""
        ));
    }

    #[test]
    fn fish_overlay_verb_loop_is_generated_from_session_id_verbs() {
        let overlay = fish_overlay();
        let expected = format!("for __felis_sub in {}", SESSION_ID_VERBS.join(" "));
        assert!(
            overlay.contains(&expected),
            "expected overlay to contain {expected:?}, got: {overlay}"
        );
    }

    /// `SESSION_ID_VERBS` must track the clap tree: every `sessions`
    /// subcommand whose first positional is ID-OR-PREFIX, and no other
    /// (zsh would keep working by accident: its overlay matches every
    /// `:id` slot generically; fish would not).
    #[test]
    fn session_id_verbs_match_the_clap_subcommand_tree() {
        use clap::CommandFactory as _;

        let mut cli = Cli::command();
        let sessions = cli
            .find_subcommand_mut("sessions")
            .expect("sessions subcommand exists");
        let mut derived: Vec<String> = sessions
            .get_subcommands()
            .filter(|sub| {
                sub.get_positionals()
                    .next()
                    .and_then(|arg| arg.get_value_names())
                    .is_some_and(|names| {
                        names.first().map(ToString::to_string).as_deref() == Some("ID-OR-PREFIX")
                    })
            })
            .map(|sub| sub.get_name().to_owned())
            .collect();
        derived.sort_unstable();
        let mut listed: Vec<String> = SESSION_ID_VERBS.iter().map(|v| (*v).to_owned()).collect();
        listed.sort_unstable();
        assert_eq!(
            listed, derived,
            "SESSION_ID_VERBS drifted from the clap tree",
        );
    }

    /// Every option-valued session-id slot gets a fish line, and only
    /// the local `--socket` reaches the helper: an SSH token on the
    /// line must never be forwarded (a `<TAB>` would dial ssh).
    #[test]
    fn fish_overlay_forwards_only_the_local_socket() {
        let overlay = fish_overlay();
        assert!(overlay.contains("__felis_carrier_flags"));
        assert!(overlay.contains("case --socket"));
        assert!(!overlay.contains("--host --socket"));
        assert!(!overlay.contains("ssh_args"));
        // The scanner's only output is the `--socket` pair and the
        // `--socket=` form: a fourth `echo` would be a fourth token
        // forwarded to the helper.
        assert_eq!(overlay.matches("echo -- ").count(), 3);
        assert!(!overlay.contains("set carrier --host"));
        // A retarget's `--session` routes through the target-aware
        // helper.
        assert_eq!(
            overlay
                .matches("-l from -a '(__felis_complete_sessions)'")
                .count(),
            2,
            "the switch verbs' --from and the retarget verbs' --from"
        );
        assert!(overlay.contains("-l session -a '(__felis_complete_sessions)'"));
        assert!(overlay.contains("-l session -a '(__felis_complete_retarget_target_sessions)'"));
        assert!(overlay.contains("__fish_seen_subcommand_from subscribe"));
    }

    /// The zsh overlay rewrites the option-valued slots too, not just
    /// the `:id` positionals, and forwards only the local `--socket`.
    #[test]
    fn zsh_overlay_routes_option_slots_and_forwards_only_the_local_socket() {
        assert!(ZSH_OVERLAY.contains("_felis_complete_retarget_target_sessions()"));
        assert!(ZSH_OVERLAY.contains("felis $carrier __complete-sessions"));
        assert!(ZSH_OVERLAY.contains("--socket)"));
        assert!(!ZSH_OVERLAY.contains("--host|--socket"));
        assert!(!ZSH_OVERLAY.contains("ssh_args"));
        assert!(!ZSH_OVERLAY.contains("carrier=(--host"));
        assert!(
            ZSH_OVERLAY.contains("(:ID-OR-PREFIX):_default/${match[1]}:_felis_complete_sessions")
        );
        assert!(ZSH_OVERLAY.contains(
            "(:TARGET-PREFIX):_default/${match[1]}:_felis_complete_retarget_target_sessions"
        ));
        assert!(ZSH_OVERLAY.contains("(:PREFIX):_default/${match[1]}:_felis_complete_sessions"));
    }

    /// A `felis` stand-in on `PATH` that appends its argv to `calls`
    /// and answers one candidate, so a shell driving the overlay
    /// records exactly which carrier tokens reached the helper.
    #[cfg(unix)]
    struct FelisStub {
        dir: tempfile::TempDir,
    }

    #[cfg(unix)]
    impl FelisStub {
        fn new() -> Self {
            use std::io::Write as _;

            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            std::fs::create_dir(&bin).unwrap();
            let stub = bin.join("felis");
            let mut f = std::fs::File::create(&stub).unwrap();
            writeln!(
                f,
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf 'deadbeef\\tstub\\n'",
                dir.path().join("calls").display()
            )
            .unwrap();
            drop(f);
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            Self { dir }
        }

        fn path_env(&self) -> std::ffi::OsString {
            let mut path = self.dir.path().join("bin").into_os_string();
            if let Some(host) = std::env::var_os("PATH") {
                path.push(":");
                path.push(host);
            }
            path
        }

        fn script(&self, name: &str, shell: Shell) -> std::path::PathBuf {
            let file = self.dir.path().join(name);
            std::fs::write(&file, script(shell)).unwrap();
            file
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("calls"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    #[cfg(unix)]
    fn shell_on_path(shell: &str) -> bool {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(shell).is_file()))
    }

    /// Command lines for `<TAB>` testing and expected helper invocations.
    ///
    /// Global `--host` and retarget SSH destinations silence helper runs.
    /// Default and `--socket` lines complete locally; a socket path on
    /// `window retarget` names the daemon its `--session` lives on.
    #[cfg(unix)]
    const TAB_LINES: &[(&str, Option<&str>)] = &[
        ("felis --host box sessions info ", None),
        ("felis --host box window retarget --from ", None),
        ("felis --host=box attach ", None),
        (
            "felis --host box --ssh-arg -p --ssh-arg 2222 sessions send ",
            None,
        ),
        ("felis --ssh-arg retarget --host box sessions info ", None),
        ("felis ssh box --session ", None),
        ("felis ssh box --ssh-arg -p --session ", None),
        ("felis sessions info ", Some("__complete-sessions")),
        ("felis ssh box --from ", Some("__complete-sessions")),
        (
            "felis --socket /x sessions info ",
            Some("--socket /x __complete-sessions"),
        ),
        (
            "felis --socket=/x attach ",
            Some("--socket=/x __complete-sessions"),
        ),
        (
            "felis window retarget /y --session ",
            Some("--socket /y __complete-sessions"),
        ),
        (
            "felis window retarget /y --format json --session ",
            Some("--socket /y __complete-sessions"),
        ),
    ];

    /// Drives the generated fish script through `complete -C` (which
    /// hands the line to `commandline` exactly as an interactive
    /// `<TAB>` would) against a recording `felis` stub. Skips when
    /// `fish` is not on `PATH`; the dev shell carries it so CI runs it.
    #[test]
    #[cfg(unix)]
    fn fish_completion_never_invokes_the_helper_with_an_ssh_carrier() {
        if !shell_on_path("fish") {
            eprintln!("skipping: fish not on PATH");
            return;
        }
        let stub = FelisStub::new();
        let script = stub.script("felis.fish", Shell::Fish);
        for (line, expected) in TAB_LINES {
            let out = std::process::Command::new("fish")
                .args([
                    "--no-config",
                    "-c",
                    &format!("source '{}'; complete -C '{line}'", script.display()),
                ])
                .env("PATH", stub.path_env())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "fish failed on {line:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let calls = stub.calls();
            assert_eq!(
                calls
                    .last()
                    .filter(|_| expected.is_some())
                    .map(String::as_str),
                *expected,
                "helper calls after {line:?}: {calls:?}"
            );
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert_eq!(
                stdout.contains("deadbeef"),
                expected.is_some(),
                "candidates for {line:?}: {stdout:?}"
            );
        }
        let calls = stub.calls();
        assert_eq!(
            calls.len(),
            TAB_LINES.iter().filter(|(_, e)| e.is_some()).count()
        );
        assert!(
            calls
                .iter()
                .all(|c| !c.contains("--host") && !c.contains("--ssh-arg")),
            "an SSH carrier token reached the helper: {calls:?}"
        );
    }

    /// The host completer belongs to `felis ssh`'s destination slot
    /// alone. Once the destination is typed, a `<TAB>` must fall
    /// through to fish's own file completion instead of offering a
    /// second host, there and past the `--` separator alike.
    #[test]
    #[cfg(unix)]
    fn fish_offers_hosts_only_in_the_ssh_destination_slot() {
        if !shell_on_path("fish") {
            eprintln!("skipping: fish not on PATH");
            return;
        }
        let stub = FelisStub::new();
        let script = stub.script("felis.fish", Shell::Fish);
        // A `$HOME` of its own, so the candidate set is this one
        // known host rather than the developer's `~/.ssh`.
        let home = stub.dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(
            home.join(".ssh").join("known_hosts"),
            "felis-test-host ssh-rsa AAAAB3\n",
        )
        .unwrap();

        for (line, offers_hosts) in [
            ("felis ssh ", true),
            ("felis ssh --ssh-arg -p ", true),
            ("felis ssh --ssh-arg=-p ", true),
            ("felis ssh devbox ", false),
            ("felis ssh devbox -- echo ", false),
            ("felis window retarget ", false),
        ] {
            let out = std::process::Command::new("fish")
                .args([
                    "--no-config",
                    "-c",
                    &format!("source '{}'; complete -C '{line}'", script.display()),
                ])
                .env("PATH", stub.path_env())
                .env("HOME", &home)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "fish failed on {line:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert_eq!(
                stdout.contains("felis-test-host"),
                offers_hosts,
                "host candidates for {line:?}: {stdout:?}"
            );
        }
    }

    /// Same contract for zsh. There is no `complete -C` equivalent, so
    /// the test sources the script under `zsh -f` with `compdef` and
    /// `_describe` stubbed and calls the two helpers with `words` set
    /// as the completion system would. Skips when `zsh` is not on
    /// `PATH`; the dev shell carries it so CI runs it.
    #[test]
    #[cfg(unix)]
    fn zsh_completion_never_invokes_the_helper_with_an_ssh_carrier() {
        use std::fmt::Write as _;

        if !shell_on_path("zsh") {
            eprintln!("skipping: zsh not on PATH");
            return;
        }
        let stub = FelisStub::new();
        let script = stub.script("_felis", Shell::Zsh);
        // Re-assert `path` inside the driver because `zsh -f` reads `/etc/zshenv`,
        // which on NixOS replaces `PATH` outright. Without this, a lost stub
        // would turn into an empty roster indistinguishable from a refusal.
        let mut driver = format!(
            "emulate -L zsh\npath=('{}' $path)\nwhence -p felis >/dev/null || {{ print -ru2 -- \"the felis stub is not on PATH: $PATH\"; exit 1 }}\ncompdef() {{ : }}\n_describe() {{ print -r -- \"describe:${{(j: :)${{(P)4}}}}\" }}\nsource '{}'\n",
            stub.dir.path().join("bin").display(),
            script.display()
        );
        for (line, _) in TAB_LINES {
            let helper = if line.contains("--session ") {
                "_felis_complete_retarget_target_sessions"
            } else {
                "_felis_complete_sessions"
            };
            let words: Vec<String> = line.split(' ').map(|w| format!("'{w}'")).collect();
            writeln!(
                driver,
                "words=({}); {helper}; print -r -- \"end:{line}\"",
                words.join(" ")
            )
            .unwrap();
        }
        let driver_path = stub.dir.path().join("drive.zsh");
        std::fs::write(&driver_path, driver).unwrap();
        let out = std::process::Command::new("zsh")
            .arg("-f")
            .arg(&driver_path)
            .env("PATH", stub.path_env())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "zsh failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut calls = stub.calls().into_iter();
        // `rest` is consumed marker by marker: splitting the whole
        // stdout each time would let one line's candidates satisfy
        // every later assertion.
        let mut rest = stdout.as_ref();
        for (line, expected) in TAB_LINES {
            let end = format!("end:{line}\n");
            let (chunk, tail) = rest
                .split_once(&end)
                .unwrap_or_else(|| panic!("no end marker for {line:?} in {stdout:?}"));
            rest = tail;
            assert_eq!(
                chunk.contains("describe:deadbeef:stub"),
                expected.is_some(),
                "candidates for {line:?}: {chunk:?} (helper calls so far: {:?}, stderr: {})",
                stub.calls(),
                String::from_utf8_lossy(&out.stderr)
            );
            if let Some(expected) = expected {
                assert_eq!(
                    calls.next().as_deref(),
                    Some(*expected),
                    "helper call for {line:?}"
                );
            }
        }
        assert_eq!(calls.next(), None, "an unexpected helper call was recorded");
    }

    /// An SSH carrier returns at once without spawning `ssh`.
    ///
    /// Uses a `ProxyCommand` marker to verify `ssh` is not spawned.
    #[test]
    fn complete_sessions_refuses_the_ssh_carrier_without_dialing() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ssh-ran");
        let target = Reconnector {
            carrier: Carrier::Ssh {
                destination: "nowhere".to_owned(),
                ssh_args: vec![
                    "-o".to_owned(),
                    format!("ProxyCommand=touch '{}'", marker.display()),
                ],
            },
            offer: felis_client_core::Offer::ops(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let started = std::time::Instant::now();
        assert_eq!(run_complete_sessions(&runtime, &target), 0);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(!marker.exists(), "ssh was spawned");
    }

    /// The zsh substitutions anchor on clap value names; a renamed
    /// `value_name` would otherwise no-op the overlay silently.
    #[test]
    fn zsh_base_script_contains_the_value_name_anchors() {
        let mut buf = Vec::new();
        let mut cmd = Cli::command();
        generate(Shell::Zsh, &mut cmd, "felis", &mut buf);
        let script = String::from_utf8(buf).unwrap();
        for anchor in [
            ":id -- ",
            ":ID-OR-PREFIX:_default",
            ":TARGET-PREFIX:_default",
            ":PREFIX:_default",
        ] {
            assert!(script.contains(anchor), "missing zsh anchor: {anchor}");
        }
    }

    /// The finite option vocabularies reach every generated script, not
    /// just zsh's. `docs/reference/cli.md` promises `--format` and
    /// `sessions capture --source` complete to their listed values in
    /// bash, zsh and fish alike.
    #[test]
    fn every_shell_script_offers_the_finite_option_values() {
        fn candidates(shell: Shell, value_name: &str, values: &[&str]) -> String {
            match shell {
                Shell::Bash => format!("-W \"{}\"", values.join(" ")),
                Shell::Zsh => format!(":{value_name}:({})", values.join(" ")),
                Shell::Fish => format!(
                    "-a \"{}\"",
                    values
                        .iter()
                        .map(|value| format!("{value}\\t''"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
                other => panic!("unhandled shell: {other:?}"),
            }
        }

        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let script = script(shell);
            for (value_name, values) in [
                ("FORMAT", &["human", "json"][..]),
                ("FORMAT", &["human", "jsonl"][..]),
                (
                    "SOURCE",
                    &["visible", "scrollback", "command-output", "last-command"][..],
                ),
            ] {
                let expected = candidates(shell, value_name, values);
                assert!(
                    script.contains(&expected),
                    "missing {shell:?} candidates: {expected}"
                );
            }
        }
    }

    #[test]
    fn zsh_overlay_routes_both_destination_spellings_to_the_hosts_completer() {
        // One anchor for both the global `--host` and `felis ssh`'s
        // positional: they share the `user@host` value name.
        assert!(ZSH_OVERLAY.contains("(:user@host):_default/${match[1]}:_hosts"));
    }
}

# Surfaces this skill does not cover

None of these is needed for the three scenarios in `SKILL.md`. Each links to its section of the CLI reference; offline,
`felis <verb> --help` and `man felis-<verb>` carry the flags.

- **`felis bridge`**: one long-lived JSON-lines process for an editor plugin that would otherwise pay a process launch
  per keystroke
  ([cli.md "`felis bridge`"](https://github.com/felis-terminal/felis/blob/main/docs/reference/cli.md#felis-bridge);
  protocol in
  [ipc.md "Non-Rust clients: the stdio bridge"](https://github.com/felis-terminal/felis/blob/main/docs/reference/ipc.md#non-rust-clients-the-stdio-bridge)).
  Same `kind` vocabulary as the CLI, failures on stdout, no `--wait`, `--key` or `--mouse`. Its ops are session
  automation only (`sessions.*`, `notifications.subscribe`, `cancel`): the operator verbs (`daemon`, `config`, `fonts`,
  `doctor`, `version`), the window launches, and the shell plumbing (`completions`, `__mangen`, `__complete-sessions`)
  are CLI-only, run as point verbs with `--format json`. It admits 64 concurrent operations and 32 auxiliary links,
  bounds stream and stdout queues, rejects unknown fields and parameters (the grammar is `$defs/request` of
  `felis-bridge-v1.schema.json`), and exits `1` immediately if stdout fails. A local relative `sessions.spawn` `cwd` is
  anchored at the bridge process's directory.
- **`sessions switch`, `felis ssh <dest>`, `felis window retarget [<socket>]`**: move a live window to another session
  or daemon. They act on windows, which an agent driving a headless session has none of
  ([cli.md "Re-point a window across daemons"](https://github.com/felis-terminal/felis/blob/main/docs/reference/cli.md#re-point-a-window-across-daemons-felis-ssh-felis-window-retarget)).
  `felis ssh devbox` re-points the window it runs in at devbox's daemon; `felis --host devbox` opens a new window there
  instead, and before a headless verb the same flag names the daemon that verb dials.
- **`pipe` / `run` keymap chords** and their `FELIS_ORIGIN_SESSION_ID` / `FELIS_HOST` / `FELIS_CWD` environment:
  user-authored scripts fired from a window's `[keymap]`
  ([keybindings.md "Unbound by default"](https://github.com/felis-terminal/felis/blob/main/docs/reference/keybindings.md#unbound-by-default)).
- **`felis doctor`, `felis config check`**: GUI and config diagnostics for a human at the desk
  ([cli.md "Doctor"](https://github.com/felis-terminal/felis/blob/main/docs/reference/cli.md#doctor),
  ["Config verbs"](https://github.com/felis-terminal/felis/blob/main/docs/reference/cli.md#config-verbs)). `doctor`,
  `config check`, and `config show-effective` are Point-diagnostic verbs: exit `1` means the document or a check
  reported diagnostics. `doctor` and `config check` still write their result object; `config show-effective` writes an
  `invalid_request` error object instead. `felis doctor report` prints the same checklist plus the environment a bug
  report needs; it is a Point verb and exits `0` with failing rows. The one `doctor` row an agent reaches for,
  `daemon-sibling`, is in [remote.md](remote.md).

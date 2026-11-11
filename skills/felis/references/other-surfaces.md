# Surfaces this skill does not cover

None of these is needed for the three scenarios in `SKILL.md`. Each links to its section of the CLI reference; offline,
`felis <verb> --help` and `man felis-<verb>` carry the flags.

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

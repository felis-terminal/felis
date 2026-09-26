---
title: Drive a session beside its window
sidebar:
  order: 8
---

Steer a session from a script, an editor, or an agent while a window keeps showing it. Attaching a window is additive,
so none of the commands below disconnect it. For full CLI options, see [cli.md](../reference/cli.md).

## Integrate with editors and scripts

Editors (such as Emacs, Neovim, and Helix), scripts, and CI runners can drive sessions using the core IPC verbs:
`spawn`, `send`, `capture`, `search`, and `kill`. Any environment capable of invoking subprocesses can interact with
felis sessions:

```emacs-lisp
;; Send the active buffer region to a target REPL session.
(defun felis/send-region (id begin end)
  (interactive (list (read-string "session: ")
                     (region-beginning) (region-end)))
  (call-process-region begin end "felis" nil nil nil
                       "sessions" "send" id "-"))
```

## Switch an active window to another session

From inside a felis window, `felis sessions switch <id>` retargets that window to another session. This command provides
the CLI equivalent of the `switch_session` keybinding chord. The session being replaced is the one named by
`$FELIS_SESSION_ID`, which felis stamps into every session's environment (see
[terminal-identity.md](../reference/terminal-identity.md)). To switch a window from an external script, specify
`--from <id>`.

Select sessions in the shell using unique ID prefixes or interactive fuzzy filtering (see the picker recipe in
[tmux-workflows-without-tmux.md](tmux-workflows-without-tmux.md)):

```sh
felis sessions switch 3fa            # switch to matching ID prefix
```

When several windows mirror the origin session, felis switches the one that most recently received user input; to target
another, pass `--attachment <id>` with an attachment ID from `felis sessions info <id> --format json`
([cli.md](../reference/cli.md#verb-details)).

To drop every window but keep the session running, run `felis sessions evict <id>`.

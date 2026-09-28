---
title: Control-surface name map
sidebar:
  order: 6
---

One concept, one user-facing word, consistent across the CLI and keymap. Wire protocol names remain descriptive. For
surface placement criteria and design rationale, see
[control-surfaces.md](../explanation/architecture/control-surfaces.md). Where each global flag lands per verb is the
placement matrix in [cli.md](cli.md) "Global options".

| Concept                             | CLI verb                                                     | Keymap kind                | IpcAction              | Wire message                                                                                          |
| ----------------------------------- | ------------------------------------------------------------ | -------------------------- | ---------------------- | ----------------------------------------------------------------------------------------------------- |
| list the roster                     | `list`                                                       | -                          | -                      | `OpsToDaemonMsg::List` -> `Listed`                                                                    |
| read one session's row by prefix    | `info`                                                       | -                          | -                      | `OpsToDaemonMsg::Info` -> `InfoReply`                                                                 |
| offer session ids to a shell        | `completions <shell>`, then the hidden `__complete-sessions` | -                          | -                      | `OpsToDaemonMsg::List` -> `Listed`                                                                    |
| attach a window to a session        | `attach <id>`                                                | -                          | -                      | `SessionToDaemonMsg::Attach` -> `Attached`                                                            |
| create, then attach in-window       | -                                                            | `new_session`              | `NewSession`           | `SessionToDaemonMsg::Create { args }`                                                                 |
| create detached (pre-warm)          | `spawn`                                                      | -                          | -                      | `OpsToDaemonMsg::Spawn { args }` / `Spawned { outcome }`                                              |
| switch the window's session         | `switch`                                                     | `switch_session` (`to`)    | `SwitchSession { to }` | `SessionToDaemonMsg::Attach`; CLI relay `OpsToDaemonMsg::Switch` -> `PushMsg::Reattach` -> `Switched` |
| re-point window to another daemon   | `ssh <dest>` / `window retarget [<socket>]`                  | -                          | -                      | CLI relay `OpsToDaemonMsg::Switch` -> `PushMsg::RetargetHost` -> `Switched`                           |
| pick window moved by relay verb     | `--attachment <id>` on `switch` / `retarget`                 | -                          | -                      | `OpsToDaemonMsg::Switch.scope`: `Default` or `Attachment(id)`                                         |
| tear a session down                 | `kill`                                                       | `kill_session`             | `KillSession`          | `OpsToDaemonMsg::Destroy` / `Destroyed`                                                               |
| detach this window's session        | -                                                            | `detach` (attached window) | -                      | `SessionToDaemonMsg::Detach`                                                                          |
| evict all other clients             | `evict`                                                      | -                          | -                      | `OpsToDaemonMsg::ForceDetach` / `Detached` / `PushMsg::Evicted`                                       |
| run an external command             | `<name>` (runs `felis-<name>`)                               | -                          | -                      | none (local process execution)                                                                        |
| read a region out                   | `capture`                                                    | -                          | -                      | `RegionToDaemonMsg::Rows` -> `Row` / `RowsDone` + `ConnToClientMsg::End` / `Error`                    |
| pipe a region to a sink             | -                                                            | `pipe`                     | -                      | `RegionToDaemonMsg::Request` -> `Reply` (sinks execute client-side)                                   |
| launch command in transient session | -                                                            | `run`                      | -                      | `SessionToDaemonMsg::Create` on local daemon                                                          |
| search scrollback                   | `search`                                                     | `open_scrollback_search`   | `OpenScrollbackSearch` | `SearchToDaemonMsg::Query` / `Match` + `ConnToClientMsg::End` / `Error`                               |
| inject text                         | `send`                                                       | `send_string`              | -                      | `InputMsg::Paste` / `KeyBytes`                                                                        |
| press a key                         | `send --key`                                                 | -                          | -                      | `InputMsg::Key`                                                                                       |
| scroll the viewport                 | -                                                            | `scroll`                   | -                      | `InputMsg::Viewport` -> `GridMsg::ViewportState`                                                      |
| jump to a neighboring prompt        | -                                                            | `scroll_to_prompt`         | -                      | `InputMsg::JumpPrompt` -> `GridMsg::ViewportState`                                                    |
| await command completion            | `send --wait`                                                | -                          | -                      | Streamed `GridMsg::PromptMark` (OSC 133 `D`)                                                          |
| relay notifications                 | `notifications subscribe`                                    | -                          | -                      | `NotifyToDaemonMsg::Subscribe` / `Subscribed` / `Event` / `Lagged`                                    |
| label a session                     | `tag` (`--remove` unlabels)                                  | -                          | -                      | `OpsToDaemonMsg::Tag` / `TagsUpdated`                                                                 |
| report daemon accounting            | `daemon status`                                              | -                          | -                      | `OpsToDaemonMsg::Status` -> `StatusReply` ([cli.md](cli.md) "Daemon status")                          |
| stop or drain the daemon            | `daemon stop` (`--force`, `--when-empty`)                    | -                          | -                      | `OpsToDaemonMsg::Stop` -> `StopReply` ([cli.md](cli.md) "Daemon stop")                                |
| report build identities             | `version`                                                    | -                          | -                      | `ConnToClientMsg::Welcome.identity`                                                                   |
| check system and runtime health     | `doctor`                                                     | -                          | -                      | Read-side handshake and frontend probe ([cli.md](cli.md) "Doctor")                                    |
| inspect configuration               | `config path` / `check` / `show-effective`                   | -                          | -                      | none (local client operation; [config.md](config.md))                                                 |
| drive daemon from external client   | `bridge`                                                     | -                          | -                      | Persistent JSON-lines connection ([ipc.md](ipc.md))                                                   |

## Naming conventions

The map covers operations that reach the daemon or a CLI verb. Keymap kinds the client answers by itself (`copy`,
`paste`, `reload`, `font_size`, `toggle_fullscreen`, `unbind`) have no row, since neither of the other two columns can
hold one.

CLI flags and enum values use `kebab-case`. Keymap tokens use `snake_case` (for example, keymap `command_output` maps to
CLI `--source command-output`). Meaning is preserved across surfaces; see
[control-surfaces.md](../explanation/architecture/control-surfaces.md) "Shared conventions".

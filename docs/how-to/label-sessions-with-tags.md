---
title: Label sessions with tags
sidebar:
  order: 7
---

Give sessions labels so you can find them again by what they are for, not by their IDs. A tag is an opaque string the
daemon stores with the session; felis attaches no meaning to it, and picking and acting on tagged sessions is done with
`jq`, `fzf`, and the other `felis sessions` verbs. For every option, see [cli.md](../reference/cli.md).

## When a tag helps

- **Several agents at once.** You run three coding agents in detached sessions. Tag each `agent`, and when one of them
  notifies you, list only the agents with their last notification to find which one is waiting.
- **Everything for one project.** A dev server, a test watcher, and a shell all belong to `proj-x`. Tag them together,
  and when you finish with the project, end all of them in one pipeline.
- **A job's state.** A long job's session starts as `running`; the script that drives it swaps the tag to `done` when
  the job finishes, so a later `list --tag done` shows what is ready to read.

## Tag a session when you start it

`spawn --tag` sets the tags at creation, so the session is never listed untagged:

```sh
ID=$(felis sessions spawn --tag agent --tag work -- claude)
```

## Add and remove tags later

Positional arguments add tags; `--remove` takes them off. One call can do both, and the change applies as a whole:

```sh
felis sessions tag "$ID" proj-x                   # add "proj-x"
felis sessions tag "$ID" done --remove running    # swap "running" for "done"
```

A session holds up to 32 tags of up to 128 bytes each; a call that would exceed either limit changes nothing and exits
`1`.

## List the sessions that carry a tag

```sh
felis sessions list --tag agent
```

With `--format json`, every session object carries its `tags` array, empty when the session has none:

```sh
felis sessions list --format json | jq -r '.sessions[] | [.short_id, (.tags | join(","))] | @tsv'
```

## Pick one of them and attach

Choose among the `agent` sessions with `fzf`, showing each one's foreground process, last notification, and working
directory, with a live capture as the preview:

```sh
felis sessions list --tag agent --format json \
  | jq -r '.sessions[] | [.id, (.foreground // "-"), (.last_notification.body // "-"), (.cwd // "-")] | @tsv' \
  | fzf --with-nth=2.. --preview 'felis sessions capture {1} --ansi' \
  | cut -f1 \
  | xargs -r felis attach
```

## End every session with a tag

When the project is done, kill everything tagged with it:

```sh
felis sessions list --tag proj-x --format json | jq -r '.sessions[].id' | xargs -rn1 felis sessions kill
```

To reap by idle time instead of by tag, see [Reap dead or stale sessions](reap-sessions.md).

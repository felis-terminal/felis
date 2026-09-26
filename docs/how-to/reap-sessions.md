---
title: Reap dead or stale sessions
sidebar:
  order: 7
---

Find the sessions you left behind and destroy the ones you do not want: list the pool with
`felis sessions list --format json`, filter it with `jq` (by idle time or by tag), and pipe what is left into
`felis sessions kill`. A detached session runs until you end it. Parameter options are documented in
[cli.md](../reference/cli.md).

## Reap stale sessions

Terminate sessions idle for more than one hour:

```sh
felis sessions list --format json \
  | jq -r '.sessions[] | select(.idle_seconds != null and .idle_seconds > 3600) | .id' \
  | xargs -rn1 felis sessions kill
```

Attached sessions carry no `idle_seconds`. `jq` sorts `null` below every number, so `> 3600` alone already skips them;
the `!= null` test states that exclusion outright rather than leaving it to the sort order.

To end every session that carries one tag, see
[Label sessions with tags](label-sessions-with-tags.md#end-every-session-with-a-tag).

Golden `felis bridge` conversations.

Each line is `{"dir": "in"|"out"|"kill-daemon", …object…}`. `in` lines are written to a live bridge's stdin, with
`{session}` replaced by the session the harness created; `out` lines are what its stdout must carry. `kill-daemon` takes
the daemon down mid-conversation.

Replies are compared per request id, because interleaving across ids is the bridge's prerogative and order within one id
is the contract. Session ids, timestamps, and every prose or terminal-content field are masked first, so nothing per-run
is frozen here.

`UPDATE_GOLDEN=1 cargo nextest run -p felis-cli --test cli_bridge golden` rewrites the `out` lines from what the binary
produced.

# Wire-break acknowledgments

`tools/proto/compat.py` (the `proto-compat` CI job, `just proto-compat`) reads `base: <40-hex sha>` lines from this
file; everything else here is prose for the reader. The rule such a line carries is in `docs/reference/testing.md` "Wire
compatibility gates".

A `base:` line acknowledges a wire-incompatible `felis.proto` change against that revision. Add one, with the why, for
an intended pre-release break; it counts only for the change that adds it, and stops matching once the schema moves past
it, so stale lines are inert and can be pruned at leisure.

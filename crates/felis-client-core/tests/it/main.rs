//! Every felis-client-core integration test, linked as one binary, but
//! `hostile_claims` and `session_memory`: each installs dhat's global
//! allocator, and dhat allows one profiler per process.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "json")]
mod json_protocol;
mod proptest_cluster_convergence;
mod proptest_row_delta_batch;
mod rehydration_snapshot;
#[cfg(all(unix, feature = "daemon-tests", feature = "json"))]
mod relay;
mod shadow_table_gc;
mod support;
#[cfg(all(unix, feature = "daemon-tests"))]
mod transient_spawn;

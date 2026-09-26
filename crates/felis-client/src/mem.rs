//! Startup RSS milestones logged at `info` on `felis::mem`.
//!
//! Unlike heap profiling, RSS captures the wgpu atlas and driver-allocated memory,
//! including memory-mapped regions on unified memory architectures.

/// A failed sample (unsupported platform) is silently skipped.
pub(crate) fn log_rss(milestone: &str) {
    const MB: f64 = 1024.0 * 1024.0;
    let Some(usage) = memory_stats::memory_stats() else {
        return;
    };
    tracing::info!(
        target: "felis::mem",
        milestone,
        rss_mb = usage.physical_mem as f64 / MB,
        virt_mb = usage.virtual_mem as f64 / MB,
        "memory footprint",
    );
}

//! Bounded-backoff retry for transient connect failures.

use std::{future::Future, num::NonZeroU32, time::Duration};

pub(crate) const fn nonzero(n: u32) -> NonZeroU32 {
    match NonZeroU32::new(n) {
        Some(n) => n,
        None => panic!("max_attempts must be non-zero"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Includes the first attempt; `1` means no retry.
    pub max_attempts: NonZeroU32,
}

impl RetryPolicy {
    /// Wait window for a freshly spawned `felis-daemon serve` to bind its
    /// socket, shared by the client's local autospawn
    /// (`felis-client-core::spawn`) and the SSH relay (`felis-daemon
    /// relay`). A daemon binds in ~4 ms, so the first sleep is 2 ms; the
    /// attempt count keeps the total window near 5.5 s.
    pub const DAEMON_BOOT: Self = Self {
        initial_backoff: Duration::from_millis(2),
        max_backoff: Duration::from_millis(200),
        max_attempts: nonzero(34),
    };

    /// Wait window for a daemon another launcher's systemd user unit is
    /// starting (`felis-client-core`'s hand-off). Sized to the unit's
    /// `TimeoutStartSec=15s`: the losing launcher must outwait the start
    /// job rather than fork a second daemon into its own cgroup, which is
    /// the placement the hand-off exists to avoid.
    pub const MANAGED_BOOT: Self = Self {
        initial_backoff: Duration::from_millis(2),
        max_backoff: Duration::from_millis(200),
        max_attempts: nonzero(82),
    };

    /// A window whose transport dropped re-dials the same carrier and
    /// session (`architecture/session-lifecycle.md` "Transport loss").
    /// Sized for the SSH carrier: each attempt spawns a fresh `ssh`, so
    /// the first sleep is a second rather than milliseconds, and the
    /// schedule spans ~23 s before the window closes.
    pub const WINDOW_RECONNECT: Self = Self {
        initial_backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(8),
        max_attempts: nonzero(6),
    };

    #[must_use]
    pub fn total_backoff(&self) -> Duration {
        let mut backoff = self.initial_backoff;
        let mut total = Duration::ZERO;
        for _ in 1..self.max_attempts.get() {
            total += backoff;
            backoff = (backoff * 2).min(self.max_backoff);
        }
        total
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
            max_attempts: nonzero(10),
        }
    }
}

/// Carries the last failure.
#[derive(Debug, thiserror::Error)]
#[error("retry exhausted after {attempts} attempt(s); last error: {source}")]
pub struct RetryError<E> {
    pub attempts: u32,
    #[source]
    pub source: E,
}

pub async fn retry_with_backoff<T, E, F, Fut>(
    op: F,
    policy: RetryPolicy,
) -> Result<T, RetryError<E>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    retry_while(op, policy, |_| true).await
}

pub async fn retry_while<T, E, F, Fut, P>(
    mut op: F,
    policy: RetryPolicy,
    should_retry: P,
) -> Result<T, RetryError<E>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    P: Fn(&E) -> bool,
{
    let max_attempts = policy.max_attempts.get();
    let mut backoff = policy.initial_backoff;
    let mut attempts: u32 = 0;
    let mut last: Option<E> = None;
    while attempts < max_attempts {
        attempts += 1;
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                let retry = should_retry(&err);
                last = Some(err);
                if !retry || attempts >= max_attempts {
                    break;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(policy.max_backoff);
            }
        }
    }
    let Some(source) = last else {
        unreachable!("loop runs at least once when max_attempts >= 1");
    };
    Err(RetryError { attempts, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn the_daemon_boot_wait_catches_a_bind_within_a_few_milliseconds() {
        // The attempt before the spawn cannot succeed, and a daemon binds
        // in ~4 ms.
        let p = RetryPolicy::DAEMON_BOOT;
        let second = p.initial_backoff;
        let third = second + second * 2;
        assert!(
            third <= Duration::from_millis(10),
            "third attempt at {third:?}"
        );
    }

    #[test]
    fn shortening_the_first_sleep_did_not_shorten_the_boot_window() {
        // The schedule must still wait out a slow bind.
        let window = RetryPolicy::DAEMON_BOOT.total_backoff();
        assert!(
            (Duration::from_secs(5)..Duration::from_secs(7)).contains(&window),
            "boot window {window:?}",
        );
    }

    #[test]
    fn the_managed_boot_window_outwaits_the_units_start_timeout() {
        let window = RetryPolicy::MANAGED_BOOT.total_backoff();
        assert!(
            (Duration::from_secs(15)..Duration::from_secs(16)).contains(&window),
            "managed boot window {window:?}",
        );
    }

    #[test]
    fn the_reconnect_default_still_gives_up_inside_half_a_minute() {
        // A hung daemon must not pin the user's window for minutes.
        let window = RetryPolicy::default().total_backoff();
        assert_eq!(window, Duration::from_millis(21_300));
    }

    proptest::proptest! {
        /// `total_backoff` is what the policy constants are sized
        /// against; the loop itself is what waits. A drift between them
        /// silently resizes every boot and reconnect window.
        #[test]
        fn the_loop_waits_exactly_the_schedule_total_backoff_advertises(
            initial_ms in 1_u64..=1_000,
            max_ms in 1_u64..=5_000,
            max_attempts in 1_u32..=12,
            stop_at in 1_u32..=15,
            fatal in proptest::bool::ANY,
            keep_failing in proptest::bool::ANY,
        ) {
            let policy = RetryPolicy {
                initial_backoff: Duration::from_millis(initial_ms),
                max_backoff: Duration::from_millis(max_ms),
                max_attempts: nonzero(max_attempts),
            };
            let stop = if keep_failing { u32::MAX } else { stop_at };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .expect("a time-paused runtime");
            let calls = Arc::new(AtomicU32::new(0));
            let counter = Arc::clone(&calls);
            let (ok, last, elapsed) = runtime.block_on(async move {
                let start = tokio::time::Instant::now();
                let outcome = retry_while(
                    || {
                        let counter = Arc::clone(&counter);
                        async move {
                            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                            if n >= stop && !fatal {
                                Ok(n)
                            } else {
                                Err((n, n >= stop && fatal))
                            }
                        }
                    },
                    policy,
                    |(_, fatal): &(u32, bool)| !*fatal,
                )
                .await;
                let elapsed = tokio::time::Instant::now() - start;
                let (ok, last) = match outcome {
                    Ok(_) => (true, None),
                    Err(err) => (false, Some((err.attempts, err.source.0))),
                };
                (ok, last, elapsed)
            });

            let expected_attempts = stop.min(max_attempts);
            proptest::prop_assert_eq!(calls.load(Ordering::SeqCst), expected_attempts);
            proptest::prop_assert_eq!(ok, !keep_failing && !fatal && stop_at <= max_attempts);
            if let Some((attempts, source)) = last {
                proptest::prop_assert_eq!(attempts, expected_attempts);
                proptest::prop_assert_eq!(source, expected_attempts, "the last error must be the one carried");
            }
            let waited = RetryPolicy {
                max_attempts: nonzero(expected_attempts),
                ..policy
            };
            proptest::prop_assert_eq!(elapsed, waited.total_backoff());
            proptest::prop_assert!(elapsed <= policy.total_backoff());
        }
    }
}

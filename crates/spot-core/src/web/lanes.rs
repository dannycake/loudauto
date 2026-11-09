//! Two request lanes. User requests go out at once. Background requests go out at
//! least a gap apart, so browsing a large library never bursts Spotify's internal
//! endpoints. Prefetch is Background that is dropped, never queued, when the lane is
//! busy: a guess at what the user wants next must not delay what they asked for.

use std::time::Duration;

use tokio::{sync::Mutex, time::Instant};

/// Minimum gap between Background (and Prefetch) requests.
pub const BACKGROUND_GAP: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// The user is waiting on it: goes out at once.
    User,
    /// Waits its turn, at least the gap after the previous background admission.
    Background,
    /// Background, but refused instead of waiting when the lane is busy.
    Prefetch,
}

pub struct Lanes {
    gap: Duration,
    /// When the previous Background or Prefetch request was admitted.
    last: Mutex<Option<Instant>>,
}

impl Lanes {
    pub fn new(gap: Duration) -> Self {
        Self {
            gap,
            last: Mutex::new(None),
        }
    }

    /// Waits for `lane`'s turn. Returns false only for a Prefetch the lane refused;
    /// the caller must then not send the request.
    pub async fn admit(&self, lane: Lane) -> bool {
        match lane {
            Lane::User => true,
            Lane::Background => {
                // Holding the lock across the sleep is what serializes Background callers.
                let mut last = self.last.lock().await;
                if let Some(prev) = *last {
                    tokio::time::sleep_until(prev + self.gap).await;
                }
                *last = Some(Instant::now());
                true
            }
            Lane::Prefetch => {
                // A locked mutex means a Background request is waiting: the lane is busy.
                let Ok(mut last) = self.last.try_lock() else {
                    return false;
                };
                if last.is_some_and(|prev| prev.elapsed() < self.gap) {
                    return false;
                }
                *last = Some(Instant::now());
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio::time::{Instant, sleep};

    use super::*;

    const GAP: Duration = Duration::from_millis(250);

    #[tokio::test(start_paused = true)]
    async fn user_is_immediate() {
        let lanes = Lanes::new(GAP);
        let start = Instant::now();
        for _ in 0..100 {
            assert!(lanes.admit(Lane::User).await);
        }
        assert_eq!(Instant::now(), start);
    }

    #[tokio::test(start_paused = true)]
    async fn background_is_spaced() {
        let lanes = Lanes::new(GAP);
        let start = Instant::now();
        for _ in 0..3 {
            assert!(lanes.admit(Lane::Background).await);
        }
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(500), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(600), "{elapsed:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn prefetch_dropped_when_busy() {
        let lanes = Lanes::new(GAP);
        let t0 = Instant::now();
        assert!(lanes.admit(Lane::Background).await);
        sleep(Duration::from_millis(100)).await;
        assert!(
            !lanes.admit(Lane::Prefetch).await,
            "t0+100 ms is inside the gap"
        );
        sleep(Duration::from_millis(200)).await;
        assert!(
            lanes.admit(Lane::Prefetch).await,
            "t0+300 ms is past the gap"
        );
        // Neither Prefetch made the caller wait.
        assert_eq!(t0.elapsed(), Duration::from_millis(300));
    }

    #[tokio::test(start_paused = true)]
    async fn prefetch_counts_as_an_admission() {
        let lanes = Lanes::new(GAP);
        let t0 = Instant::now();
        assert!(lanes.admit(Lane::Prefetch).await);
        assert!(lanes.admit(Lane::Background).await);
        assert_eq!(t0.elapsed(), GAP);
    }

    #[tokio::test(start_paused = true)]
    async fn prefetch_never_queues_behind_a_waiting_background() {
        let lanes = Arc::new(Lanes::new(GAP));
        assert!(lanes.admit(Lane::Background).await);
        let waiter = {
            let lanes = Arc::clone(&lanes);
            tokio::spawn(async move { lanes.admit(Lane::Background).await })
        };
        tokio::task::yield_now().await;
        let start = Instant::now();
        assert!(!lanes.admit(Lane::Prefetch).await);
        assert_eq!(Instant::now(), start);
        assert!(waiter.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn user_not_delayed_by_background() {
        let lanes = Arc::new(Lanes::new(GAP));
        assert!(lanes.admit(Lane::Background).await);
        let waiter = {
            let lanes = Arc::clone(&lanes);
            tokio::spawn(async move {
                assert!(lanes.admit(Lane::Background).await);
                Instant::now()
            })
        };
        // Let the second Background reach its wait.
        tokio::task::yield_now().await;
        let start = Instant::now();
        assert!(lanes.admit(Lane::User).await);
        let user_done = Instant::now();
        assert_eq!(user_done, start);
        assert!(!waiter.is_finished());
        let background_done = waiter.await.unwrap();
        assert!(
            user_done < background_done,
            "{user_done:?} vs {background_done:?}"
        );
    }
}

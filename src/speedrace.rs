//! Speed race: some identical copies of one request, of which the copy that
//! streams fastest is kept.
//!
//! A provider serves a model from replicas of different speed. On DeepSeek
//! (2026-11-18, 345 file streams) a stream ran at about 240 tokens/s or at
//! about 350-420, with almost nothing between, and the class showed at once:
//! of 135 streams, each one above 280 tokens/s in its first 1.5 s stayed
//! fast, and no slow one was above it. So the copies stream for a short
//! window from the first text of any of them, the copy with the most text
//! then is kept, and the others are dropped (their streams close). A copy
//! that finishes inside the window wins as it is.
//!
//! The copies carry the same request and the choice looks at speed only, so
//! the answer is drawn as without the race. The cost is the text that the
//! dropped copies wrote in the window.
//!
//! Off unless a host calls [`speed_race`] (the bench switch `speedrace`).

use anyhow::Result;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What one copy tells the race, and what the race tells the copy.
#[derive(Debug, Default)]
pub struct Progress {
    /// Characters of text received so far.
    chars: AtomicUsize,
    /// Milliseconds from the race's start to the copy's first text, plus one
    /// (0: none yet).
    first_ms: AtomicU64,
    /// The copy is the kept one: it may show its text.
    live: AtomicBool,
}

impl Progress {
    /// The copy has `chars` characters of text, `since` the race's start.
    pub fn report(&self, chars: usize, since: Duration) {
        if chars > 0 && self.first_ms.load(Ordering::Relaxed) == 0 {
            let _ = self.first_ms.compare_exchange(
                0,
                since.as_millis() as u64 + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        self.chars.store(chars, Ordering::Relaxed);
    }

    /// True once the copy is the kept one. Until then it must not show its
    /// text (another copy may be kept).
    pub fn live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }
}

/// What a race did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpeedOutcome {
    /// Copies started.
    pub copies: usize,
    /// The kept copy.
    pub winner: usize,
    /// True when the window chose (false: a copy finished or the others
    /// failed first).
    pub chosen: bool,
    /// Characters that the dropped copies had written.
    pub dropped_chars: usize,
}

/// Run `copies` copies of a request (`start(copy, progress, began)`; each
/// reports its text to its [`Progress`] and shows it only once
/// [`Progress::live`]) and keep one: the first to finish, or the copy with
/// the most text `window` after the first text of any copy. A copy that
/// fails leaves the race to the others; the last error is returned when all
/// fail. One copy runs as it is, live from the start.
pub async fn speed_race<T, F, Fut>(
    copies: usize,
    window: Duration,
    mut start: F,
) -> (Result<T>, SpeedOutcome)
where
    F: FnMut(usize, Arc<Progress>, Instant) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let began = Instant::now();
    let copies = copies.max(1);
    let progress: Vec<Arc<Progress>> = (0..copies).map(|_| Arc::default()).collect();
    let mut outcome = SpeedOutcome {
        copies,
        ..SpeedOutcome::default()
    };
    if copies == 1 {
        progress[0].live.store(true, Ordering::Relaxed);
        return (start(0, progress[0].clone(), began).await, outcome);
    }
    let mut running: Vec<(usize, Pin<Box<Fut>>)> = (0..copies)
        .map(|copy| (copy, Box::pin(start(copy, progress[copy].clone(), began))))
        .collect();
    let mut decided = false;
    loop {
        // One copy left: it is the kept one.
        if !decided && running.len() == 1 {
            progress[running[0].0].live.store(true, Ordering::Relaxed);
            outcome.winner = running[0].0;
            decided = true;
        }
        let tick = tokio::time::sleep(Duration::from_millis(40));
        let any = futures::future::select_all(running.iter_mut().map(|(_, copy)| copy.as_mut()));
        tokio::select! {
            (result, index, _) = any => {
                let (copy, _) = running.remove(index);
                match result {
                    Ok(done) => {
                        outcome.winner = copy;
                        outcome.dropped_chars += running
                            .iter()
                            .map(|(other, _)| progress[*other].chars.load(Ordering::Relaxed))
                            .sum::<usize>();
                        return (Ok(done), outcome);
                    }
                    // The others race on; the last failure is the result.
                    Err(error) if running.is_empty() => return (Err(error), outcome),
                    Err(_) => {}
                }
            }
            () = tick => {}
        }
        if decided {
            continue;
        }
        let first = running
            .iter()
            .map(|(copy, _)| progress[*copy].first_ms.load(Ordering::Relaxed))
            .filter(|ms| *ms > 0)
            .min();
        let Some(first) = first else {
            continue;
        };
        if began.elapsed() < Duration::from_millis(first - 1) + window {
            continue;
        }
        let best = running
            .iter()
            .map(|(copy, _)| *copy)
            .max_by_key(|copy| progress[*copy].chars.load(Ordering::Relaxed))
            .unwrap_or(running[0].0);
        outcome.dropped_chars += running
            .iter()
            .filter(|(copy, _)| *copy != best)
            .map(|(copy, _)| progress[*copy].chars.load(Ordering::Relaxed))
            .sum::<usize>();
        running.retain(|(copy, _)| *copy == best);
        progress[best].live.store(true, Ordering::Relaxed);
        outcome.winner = best;
        outcome.chosen = true;
        decided = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copy that writes `rate` characters each 10 ms, `total` in all,
    /// after `wait` ms.
    async fn writer(
        progress: Arc<Progress>,
        began: Instant,
        wait: u64,
        rate: usize,
        total: usize,
        shown: Arc<AtomicUsize>,
        copy: usize,
    ) -> Result<usize> {
        tokio::time::sleep(Duration::from_millis(wait)).await;
        let mut chars = 0;
        while chars < total {
            tokio::time::sleep(Duration::from_millis(10)).await;
            chars = (chars + rate).min(total);
            progress.report(chars, began.elapsed());
            if progress.live() {
                shown.store(copy + 1, Ordering::Relaxed);
            }
        }
        Ok(copy)
    }

    #[tokio::test]
    async fn the_copy_with_the_most_text_after_the_window_is_kept() {
        let shown = Arc::new(AtomicUsize::new(0));
        let (result, outcome) =
            speed_race(3, Duration::from_millis(100), |copy, progress, began| {
                // Copy 1 starts later and writes faster.
                let (wait, rate) = [(0, 2), (20, 5), (0, 1)][copy];
                writer(progress, began, wait, rate, 400, shown.clone(), copy)
            })
            .await;
        assert_eq!(result.unwrap(), 1);
        assert_eq!(outcome.winner, 1);
        assert!(outcome.chosen);
        assert_eq!(outcome.copies, 3);
        assert!(outcome.dropped_chars > 0);
        // Only the kept copy showed its text.
        assert_eq!(shown.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn a_copy_that_finishes_inside_the_window_wins_and_a_failed_copy_leaves_the_race() {
        let shown = Arc::new(AtomicUsize::new(0));
        let (result, outcome) =
            speed_race(2, Duration::from_millis(500), |copy, progress, began| {
                writer(progress, began, 0, [1, 4][copy], 40, shown.clone(), copy)
            })
            .await;
        assert_eq!(result.unwrap(), 1);
        assert!(!outcome.chosen);

        let (result, outcome) =
            speed_race(2, Duration::from_millis(50), |copy, progress, began| {
                let shown = shown.clone();
                async move {
                    if copy == 0 {
                        anyhow::bail!("refused");
                    }
                    writer(progress, began, 0, 1, 30, shown, copy).await
                }
            })
            .await;
        assert_eq!(result.unwrap(), 1);
        assert_eq!(outcome.winner, 1);

        let (result, _) = speed_race(2, Duration::from_millis(50), |_, _, _| async {
            anyhow::bail!("refused");
            #[allow(unreachable_code)]
            Ok(0usize)
        })
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn one_copy_is_live_from_the_start() {
        let (result, outcome) =
            speed_race(1, Duration::from_millis(50), |_, progress, _| async move {
                Ok(progress.live())
            })
            .await;
        assert!(result.unwrap());
        assert_eq!(outcome.copies, 1);
    }
}

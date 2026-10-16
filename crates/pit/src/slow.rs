//! Noticing CI that gets slower. CI services report a failed run, but a task
//! that quietly went from 1 minute to 4 gets nobody's attention until the
//! whole pipeline is slow. Pit compares each passing task with its own
//! history: a run much slower than usual says so in its result, and
//! `pit slow` lists tasks whose recent runs are slower than earlier ones.
//!
//! Both checks need a clear margin, in ratio and in absolute time, so normal
//! jitter on short tasks never shows up.

/// Earlier passing runs needed before a run is called slow.
pub const MIN_HISTORY: usize = 5;
/// How many earlier runs "usually" is taken from.
pub const HISTORY: usize = 10;

/// A single run counts as slow at this ratio over the median and this many
/// milliseconds more.
const RUN_RATIO: f64 = 1.5;
const RUN_MS: i64 = 2_000;
/// A trend counts at this ratio between the last runs and the ones before.
const TREND_RATIO: f64 = 1.3;
const TREND_MS: i64 = 1_000;
/// Runs that make up "recent" in a trend.
const RECENT: usize = 5;

pub fn median(values: &[i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let mid = v.len() / 2;
    Some(if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2
    } else {
        v[mid]
    })
}

/// The note for a run that took `ms` when its earlier runs took `history`,
/// if it is clearly slower than usual.
pub fn note(ms: i64, history: &[i64]) -> Option<String> {
    if history.len() < MIN_HISTORY {
        return None;
    }
    let usual = median(history)?;
    if (ms as f64) < usual as f64 * RUN_RATIO || ms - usual < RUN_MS {
        return None;
    }
    Some(format!(
        "slow: took {:.1}s, usually {:.1}s ({:.1}x, median of its last {} passing runs)",
        ms as f64 / 1000.0,
        usual as f64 / 1000.0,
        ms as f64 / usual.max(1) as f64,
        history.len()
    ))
}

/// The median of the last `RECENT` durations and of the ones before them,
/// oldest first in `durations`, if the recent ones are clearly slower.
pub fn trend(durations: &[i64]) -> Option<(i64, i64)> {
    if durations.len() < RECENT + MIN_HISTORY {
        return None;
    }
    let (before, recent) = durations.split_at(durations.len() - RECENT);
    let (recent, before) = (median(recent)?, median(before)?);
    let slower = recent as f64 >= before as f64 * TREND_RATIO && recent - before >= TREND_MS;
    slower.then_some((recent, before))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clearly_slow_run_is_noted_and_jitter_is_not() {
        let usual = [1_000, 1_100, 900, 1_000, 1_050];
        let n = note(4_000, &usual).unwrap();
        assert!(n.starts_with("slow: took 4.0s, usually 1.0s (4.0x"), "{n}");
        // Twice as slow but only 1s more: noise on a short task.
        assert_eq!(note(2_000, &usual), None);
        // Not enough history yet.
        assert_eq!(note(9_000, &usual[..3]), None);
        // 10% slower on a long task is not news.
        assert_eq!(note(66_000, &[60_000; 6]), None);
    }

    #[test]
    fn a_creep_shows_as_a_trend() {
        // 1s, then half a second slower every run.
        let creep: Vec<i64> = (0..10).map(|i| 1_000 + i * 500).collect();
        assert_eq!(trend(&creep), Some((4_500, 2_000)));
        assert_eq!(trend(&[1_000; 12]), None);
        assert_eq!(trend(&creep[..6]), None);
    }
}

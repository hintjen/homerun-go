//! What a running server is costing, and how much of that to remember.
//!
//! Reference: `nativeServerManager.ts`'s perf sampler — `takePerfSample`,
//! `schedulePerfSample`, `stopPerfSampling`.
//!
//! # Why this is not four implementations
//!
//! It was. The desktop samples every 30 s, keeps 360 points, and when the
//! buffer fills it *halves its own resolution* rather than losing the start of
//! the session. Android's Pumpkin backend samples every second and keeps 1800,
//! with a comment claiming it matches the desktop's window — it does not; the
//! desktop's is three hours and Android's is thirty minutes. iOS keeps 720 on
//! its log-pump tick. Android's JVM backend has no sampler at all, which is
//! why the Insights tab is empty on a phone hosting a Java server.
//!
//! Four answers to "what does this graph cover", and a graph is a claim about
//! the past that a user reads as fact. So the retention rule, the arithmetic
//! that turns two CPU counters into a percentage, and the judgement about when
//! a number is not trustworthy all live here.
//!
//! # What a host still does
//!
//! Everything platform: `/proc/<pid>/stat` on Android, `task_info` on iOS,
//! whatever the desktop asks Windows. A host reads **counters** — bytes
//! resident, seconds of CPU consumed — and hands them over. It never computes
//! a percentage, because a percentage is a difference between two moments and
//! that is where the wrong numbers come from.
//!
//! ```text
//!   host: read /proc/<pid>       →  Reading { at_ms, mem_used_kb, cpu_seconds }
//!   core: History::record(…)     →  a Sample, or nothing if it is not due yet
//!   host: answer the bridge      ←  History::samples()
//! ```
//!
//! # The state lives with the caller
//!
//! Like [`crate::lifecycle`], a `History` is serialised and held by the host
//! between calls, so there is no native allocation to free and no second copy
//! to disagree with. One per **run**, not per server: a graph covers a
//! session, and a restart starts a new one.

use serde::{Deserialize, Serialize};

/// How long to remember, and at what resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    /// How often a sample is kept. Not how often the host may *offer* one —
    /// Android's log pump ticks every second and offers every tick.
    pub interval_ms: u64,
    /// The coarsest this may become as the session runs long.
    pub max_interval_ms: u64,
    /// How many points the graph holds.
    pub max_samples: usize,
}

impl Default for Policy {
    /// The desktop's numbers, so a phone's graph and a PC's graph of the same
    /// server cover the same span.
    fn default() -> Self {
        Policy {
            interval_ms: 30_000,
            max_interval_ms: 30 * 60_000,
            max_samples: 360,
        }
    }
}

/// Counters a host read at one instant. Never percentages.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reading {
    /// Wall clock, milliseconds since the epoch. The host's, because only the
    /// host has one — this crate deliberately has no access to a clock.
    pub at_ms: i64,
    /// Resident memory in KiB, or `None` when the platform would not say.
    #[serde(default)]
    pub mem_used_kb: Option<u64>,
    /// **Cumulative** CPU seconds this process has consumed since it started.
    /// A rate is derived from two of these; a host must not pre-divide.
    #[serde(default)]
    pub cpu_seconds: Option<f64>,
    #[serde(default)]
    pub player_count: Option<u32>,
}

/// One point on a graph.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub t: i64,
    pub mem_used_mb: Option<u64>,
    /// Can exceed 100: a server uses more than one core, and clamping would
    /// hide exactly the moment worth seeing.
    pub cpu_percent: Option<f64>,
    pub player_count: Option<u32>,
}

/// One run's graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct History {
    policy: Policy,
    /// The live interval, which grows as the session outlives the buffer.
    interval_ms: u64,
    samples: Vec<Sample>,
    /// The last reading, kept only to derive the next rate from.
    #[serde(default)]
    previous: Option<Reading>,
}

impl History {
    pub fn new(policy: Policy) -> Self {
        History {
            interval_ms: policy.interval_ms,
            policy,
            samples: Vec::new(),
            previous: None,
        }
    }

    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    /// How long the host should wait before offering the next reading.
    ///
    /// Worth re-reading after every [`record`](Self::record): it doubles when
    /// the buffer fills, and a host that scheduled once on the original value
    /// keeps sampling at a resolution this has stopped keeping.
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// Whether a reading taken now would be kept.
    ///
    /// A host may offer readings as often as it likes — Android's log pump
    /// ticks every second — and asking first is cheaper than reading `/proc`
    /// to have the answer thrown away.
    pub fn due(&self, now_ms: i64) -> bool {
        match self.samples.last() {
            None => true,
            Some(last) => now_ms.saturating_sub(last.t) >= self.interval_ms as i64,
        }
    }

    /// Offer a reading. Returns whether it became a sample.
    ///
    /// The previous reading is remembered either way, because the CPU rate is
    /// a difference and losing the earlier half of it costs a whole interval.
    pub fn record(&mut self, reading: Reading) -> bool {
        let cpu_percent = self
            .previous
            .and_then(|previous| cpu_percent(&previous, &reading));

        // Kept even when the sample is not, so the *next* rate is measured
        // from the most recent counter rather than from a stale one.
        self.previous = Some(reading);

        if !self.due(reading.at_ms) {
            return false;
        }

        self.samples.push(Sample {
            t: reading.at_ms,
            mem_used_mb: reading.mem_used_kb.map(|kb| kb / 1024),
            cpu_percent,
            player_count: reading.player_count,
        });

        self.compact();
        true
    }

    /// Make room, preferring to lose resolution over losing the beginning.
    ///
    /// A session's first minutes are usually the interesting ones — a world
    /// generating, a memory curve settling — so a full buffer drops every
    /// other point and halves how often it fills again, rather than sliding a
    /// window forward and forgetting the launch. Only once the interval has
    /// grown as coarse as the policy allows does this fall back to dropping
    /// the oldest.
    fn compact(&mut self) {
        if self.samples.len() <= self.policy.max_samples {
            return;
        }

        if self.interval_ms < self.policy.max_interval_ms {
            let mut keep = 0;
            self.samples.retain(|_| {
                keep += 1;
                keep % 2 == 1
            });
            self.interval_ms = (self.interval_ms * 2).min(self.policy.max_interval_ms);
        } else {
            let excess = self.samples.len() - self.policy.max_samples;
            self.samples.drain(..excess);
        }
    }
}

/// A percentage from two cumulative CPU counters, or `None` when it cannot be
/// trusted.
///
/// Refuses rather than guesses in three cases, all of which produce a visible
/// lie on a graph:
///
///  - **no elapsed time.** Two readings in the same millisecond divide by zero.
///  - **the counter went backwards.** The process was replaced, or the platform
///    wrapped its counter. The truthful answer is "unknown", not a negative.
///  - **either side is missing.** A platform that would not say does not get
///    a number invented for it.
pub fn cpu_percent(previous: &Reading, current: &Reading) -> Option<f64> {
    let (before, after) = (previous.cpu_seconds?, current.cpu_seconds?);
    let elapsed_ms = current.at_ms.checked_sub(previous.at_ms)?;
    if elapsed_ms <= 0 {
        return None;
    }
    let used = after - before;
    if used < 0.0 {
        return None;
    }
    Some((used / (elapsed_ms as f64 / 1000.0)) * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(at_ms: i64, cpu_seconds: f64) -> Reading {
        Reading {
            at_ms,
            mem_used_kb: Some(2048 * 1024),
            cpu_seconds: Some(cpu_seconds),
            player_count: Some(3),
        }
    }

    fn history() -> History {
        History::new(Policy::default())
    }

    // --- the rate ----------------------------------------------------------

    #[test]
    fn one_core_saturated_for_the_whole_interval_is_one_hundred_percent() {
        let percent = cpu_percent(&reading(0, 10.0), &reading(30_000, 40.0)).unwrap();
        assert!((percent - 100.0).abs() < 1e-9, "{percent}");
    }

    #[test]
    fn more_than_one_core_reads_above_one_hundred() {
        // Three cores busy for 30 s of wall clock is 90 s of CPU.
        let percent = cpu_percent(&reading(0, 0.0), &reading(30_000, 90.0)).unwrap();
        assert!((percent - 300.0).abs() < 1e-9, "{percent}");
    }

    #[test]
    fn a_rate_that_cannot_be_trusted_is_not_invented() {
        // No elapsed time.
        assert_eq!(
            cpu_percent(&reading(1_000, 1.0), &reading(1_000, 2.0)),
            None
        );
        // Time going backwards.
        assert_eq!(
            cpu_percent(&reading(2_000, 1.0), &reading(1_000, 2.0)),
            None
        );
        // A counter that went backwards: the process was replaced.
        assert_eq!(cpu_percent(&reading(0, 90.0), &reading(30_000, 1.0)), None);

        // A platform that would not say.
        let quiet = Reading {
            cpu_seconds: None,
            ..reading(30_000, 0.0)
        };
        assert_eq!(cpu_percent(&reading(0, 1.0), &quiet), None);
        assert_eq!(cpu_percent(&quiet, &reading(60_000, 1.0)), None);
    }

    // --- recording ---------------------------------------------------------

    #[test]
    fn the_first_sample_has_no_rate_to_report() {
        let mut history = history();
        assert!(history.record(reading(0, 5.0)));
        assert_eq!(history.samples().len(), 1);
        assert_eq!(history.samples()[0].cpu_percent, None);
        assert_eq!(history.samples()[0].mem_used_mb, Some(2048));
        assert_eq!(history.samples()[0].player_count, Some(3));
    }

    #[test]
    fn readings_offered_faster_than_the_interval_are_dropped() {
        let mut history = history();
        assert!(history.record(reading(0, 0.0)));
        // Android's log pump ticks every second and offers every tick.
        for second in 1..30 {
            assert!(
                !history.record(reading(second * 1_000, second as f64)),
                "second {second} should not have been kept"
            );
        }
        assert!(history.record(reading(30_000, 30.0)));
        assert_eq!(history.samples().len(), 2);
    }

    #[test]
    fn a_dropped_reading_still_anchors_the_next_rate() {
        let mut history = history();
        history.record(reading(0, 0.0));
        // Dropped, but remembered: one core busy from here on.
        history.record(reading(29_000, 29.0));
        history.record(reading(30_000, 30.0));

        // Measured from 29 s, not from 0 — one second of wall clock, one
        // second of CPU.
        let percent = history.samples()[1].cpu_percent.unwrap();
        assert!((percent - 100.0).abs() < 1e-9, "{percent}");
    }

    #[test]
    fn due_answers_before_a_host_pays_to_read_proc() {
        let mut history = history();
        assert!(history.due(0), "an empty history always wants a sample");
        history.record(reading(0, 0.0));
        assert!(!history.due(29_999));
        assert!(history.due(30_000));
    }

    // --- retention ---------------------------------------------------------

    #[test]
    fn a_full_buffer_halves_its_resolution_rather_than_forgetting_the_launch() {
        let policy = Policy {
            interval_ms: 1_000,
            max_interval_ms: 8_000,
            max_samples: 4,
        };
        let mut history = History::new(policy);

        for second in 0..5 {
            history.record(reading(second * 1_000, second as f64));
        }

        // Five offered into a buffer of four: every other one kept, and the
        // first — the launch — is still there.
        assert_eq!(
            history.samples().iter().map(|s| s.t).collect::<Vec<_>>(),
            vec![0, 2_000, 4_000]
        );
        assert_eq!(history.interval_ms(), 2_000);
    }

    #[test]
    fn the_interval_stops_growing_and_then_the_oldest_goes() {
        let policy = Policy {
            interval_ms: 1_000,
            max_interval_ms: 2_000,
            max_samples: 4,
        };
        let mut history = History::new(policy);

        let mut at = 0;
        for step in 0..40 {
            at += history.interval_ms() as i64;
            history.record(reading(at, step as f64));
        }

        assert_eq!(history.interval_ms(), 2_000, "capped by the policy");
        assert_eq!(history.samples().len(), policy.max_samples);
        // Sliding now, so the launch is finally gone — but only after the
        // resolution had nowhere left to go.
        assert!(history.samples()[0].t > 0);
    }

    #[test]
    fn a_graph_never_exceeds_its_cap() {
        let mut history = history();
        let mut at = 0;
        for step in 0..2_000 {
            at += history.interval_ms() as i64;
            history.record(reading(at, step as f64));
            assert!(history.samples().len() <= Policy::default().max_samples);
        }
    }

    #[test]
    fn a_history_survives_the_round_trip_a_host_puts_it_through() {
        let mut history = history();
        history.record(reading(0, 0.0));
        history.record(reading(30_000, 30.0));

        let wire = serde_json::to_string(&history).unwrap();
        let mut back: History = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, history);

        // And the rate still anchors on the reading from before the trip.
        back.record(reading(60_000, 60.0));
        let percent = back.samples()[2].cpu_percent.unwrap();
        assert!((percent - 100.0).abs() < 1e-9, "{percent}");
    }
}

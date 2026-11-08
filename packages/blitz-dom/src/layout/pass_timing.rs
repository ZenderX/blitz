//! Opt-in timing of the per-layout passes that walk the tree on every resolve: the size
//! comparison (`box_sizes` before layout and `damage_overflow_of_resized_boxes` after it),
//! the fixed-box overflow pass (`scroll_fixed_boxes`) and the hoisting pass
//! (`place_hoisted_children`).
//!
//! With `BLITZ_LAYOUT_PASS_TIMING=1` in the environment, each `resolve_layout` writes one
//! line to stderr with the node count and the durations of the whole resolve and of each
//! pass. The variable is read once per process; when it is unset no clock is read.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

const ENV_VAR: &str = "BLITZ_LAYOUT_PASS_TIMING";

fn is_enabled() -> bool {
    static IS_ENABLED: OnceLock<bool> = OnceLock::new();
    *IS_ENABLED.get_or_init(|| std::env::var_os(ENV_VAR).is_some_and(|value| value == "1"))
}

#[derive(Clone, Copy)]
pub(crate) enum Pass {
    SizeComparison,
    FixedOverflow,
    Hoisting,
}

pub(crate) struct LayoutPassTimer {
    started: Instant,
    size_comparison: Duration,
    fixed_overflow: Duration,
    hoisting: Duration,
}

impl LayoutPassTimer {
    /// A timer started now, or `None` when timing is off.
    pub(crate) fn start() -> Option<Self> {
        is_enabled().then(|| Self {
            started: Instant::now(),
            size_comparison: Duration::ZERO,
            fixed_overflow: Duration::ZERO,
            hoisting: Duration::ZERO,
        })
    }

    fn record(&mut self, pass: Pass, elapsed: Duration) {
        let total = match pass {
            Pass::SizeComparison => &mut self.size_comparison,
            Pass::FixedOverflow => &mut self.fixed_overflow,
            Pass::Hoisting => &mut self.hoisting,
        };
        *total += elapsed;
    }

    /// Write the line for a resolve over `node_count` nodes that ends now.
    pub(crate) fn finish(self, node_count: usize) {
        eprintln!("{}", self.line(node_count, self.started.elapsed()));
    }

    fn line(&self, node_count: usize, resolve: Duration) -> String {
        format!(
            "blitz layout pass timing: nodes={node_count} resolve_ms={:.3} size_comparison_ms={:.3} fixed_overflow_ms={:.3} hoisting_ms={:.3}",
            millis(resolve),
            millis(self.size_comparison),
            millis(self.fixed_overflow),
            millis(self.hoisting),
        )
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Run `pass`, adding its duration to `timer` when timing is on.
pub(crate) fn time_pass<T>(
    timer: &mut Option<LayoutPassTimer>,
    pass: Pass,
    run: impl FnOnce() -> T,
) -> T {
    let Some(timer) = timer else {
        return run();
    };
    let started = Instant::now();
    let output = run();
    timer.record(pass, started.elapsed());
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_reports_the_node_count_and_each_duration_with_split_passes_summed() {
        let mut timer = LayoutPassTimer {
            started: Instant::now(),
            size_comparison: Duration::ZERO,
            fixed_overflow: Duration::ZERO,
            hoisting: Duration::ZERO,
        };
        // The size comparison runs once before layout and once after it.
        timer.record(Pass::SizeComparison, Duration::from_micros(250));
        timer.record(Pass::SizeComparison, Duration::from_micros(500));
        timer.record(Pass::FixedOverflow, Duration::from_micros(42));
        timer.record(Pass::Hoisting, Duration::from_micros(1_234));

        assert_eq!(
            timer.line(811, Duration::from_micros(28_000)),
            "blitz layout pass timing: nodes=811 resolve_ms=28.000 size_comparison_ms=0.750 fixed_overflow_ms=0.042 hoisting_ms=1.234",
        );
    }
}

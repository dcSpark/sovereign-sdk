//! Exponential Moving Average (EMA) computation utilities.
//!
//! This module provides EMA calculations for various time windows as specified
//! in the MockMCP authority API specification.

#![allow(dead_code)]

use std::sync::Arc;
use tokio::sync::RwLock;

/// EMA window configurations matching MockMCP spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmaWindow {
    /// 5-second EMA - fastest response, best for quick demos
    S5,
    /// 1-minute EMA - good for short-term monitoring
    M1,
    /// 5-minute EMA - balanced view for medium-term simulations
    M5,
    /// 15-minute EMA - smoothest view for long-term trends
    M15,
}

impl EmaWindow {
    /// Returns the window duration in seconds.
    pub fn seconds(&self) -> u64 {
        match self {
            EmaWindow::S5 => 5,
            EmaWindow::M1 => 60,
            EmaWindow::M5 => 300,
            EmaWindow::M15 => 900,
        }
    }

    /// Returns the smoothing factor alpha for EMA calculation.
    /// alpha = 2 / (N + 1), where N is the number of periods.
    /// For time-based EMA, we use the window in seconds as N.
    pub fn alpha(&self) -> f64 {
        let n = self.seconds() as f64;
        2.0 / (n + 1.0)
    }

    /// Returns the path suffix for this window.
    pub fn path_suffix(&self) -> &'static str {
        match self {
            EmaWindow::S5 => "s5",
            EmaWindow::M1 => "m1",
            EmaWindow::M5 => "m5",
            EmaWindow::M15 => "m15",
        }
    }
}

/// State for tracking EMA values over time.
#[derive(Clone, Debug)]
pub struct EmaState {
    /// Current EMA value.
    pub value: f64,
    /// Timestamp of last update (ms since epoch).
    pub last_update_ms: i64,
}

impl Default for EmaState {
    fn default() -> Self {
        Self {
            value: 0.0,
            last_update_ms: 0,
        }
    }
}

impl EmaState {
    /// Updates the EMA with a new observation.
    ///
    /// # Arguments
    /// * `new_value` - The new observed value
    /// * `now_ms` - Current timestamp in milliseconds
    /// * `window` - The EMA window to use
    ///
    /// # Returns
    /// The updated EMA value
    pub fn update(&mut self, new_value: f64, now_ms: i64, window: EmaWindow) -> f64 {
        let alpha = window.alpha();

        if self.last_update_ms == 0 {
            // First observation - initialize with the new value
            self.value = new_value;
        } else {
            // Calculate time-weighted alpha
            let elapsed_secs = (now_ms - self.last_update_ms).max(0) as f64 / 1000.0;
            let effective_alpha = if elapsed_secs > 0.0 {
                // Adjust alpha based on actual time elapsed vs expected interval
                let expected_interval = 1.0; // We expect updates roughly every second
                1.0 - (1.0 - alpha).powf(elapsed_secs / expected_interval)
            } else {
                alpha
            };

            // EMA formula: EMA_t = α * value_t + (1 - α) * EMA_(t-1)
            self.value = effective_alpha * new_value + (1.0 - effective_alpha) * self.value;
        }

        self.last_update_ms = now_ms;
        self.value
    }
}

/// Computes a simple EMA from a series of samples.
///
/// This is useful for computing EMA from historical data at query time.
///
/// # Arguments
/// * `samples` - Iterator of (timestamp_ms, value) pairs, must be sorted by timestamp
/// * `window` - The EMA window to use
///
/// # Returns
/// The final EMA value, or None if no samples
pub fn compute_ema_from_samples<I>(samples: I, window: EmaWindow) -> Option<f64>
where
    I: IntoIterator<Item = (i64, f64)>,
{
    let mut state = EmaState::default();
    let mut last_value = None;

    for (ts, value) in samples {
        state.update(value, ts, window);
        last_value = Some(state.value);
    }

    last_value
}

/// Computes TPS using EMA from a series of transaction count samples.
///
/// # Arguments
/// * `samples` - Iterator of (timestamp_ms, total_transactions) pairs, sorted by timestamp
/// * `window` - The EMA window to use
///
/// # Returns
/// The EMA TPS value, or None if insufficient samples
pub fn compute_tps_ema<I>(samples: I, window: EmaWindow) -> Option<f64>
where
    I: IntoIterator<Item = (i64, u64)>,
{
    let samples: Vec<_> = samples.into_iter().collect();
    if samples.len() < 2 {
        return None;
    }

    let mut state = EmaState::default();
    let mut last_tps = None;

    for i in 1..samples.len() {
        let (prev_ts, prev_count) = samples[i - 1];
        let (curr_ts, curr_count) = samples[i];

        let delta_ms = curr_ts - prev_ts;
        if delta_ms <= 0 {
            continue;
        }

        let delta_count = curr_count.saturating_sub(prev_count);
        let tps = (delta_count as f64) / (delta_ms as f64 / 1000.0);

        state.update(tps, curr_ts, window);
        last_tps = Some(state.value);
    }

    last_tps
}

/// Computes tokens per second using EMA from a series of total amount samples.
///
/// # Arguments
/// * `samples` - Iterator of (timestamp_ms, total_amount) pairs, sorted by timestamp
/// * `window` - The EMA window to use
///
/// # Returns
/// The EMA tokens/second value, or None if insufficient samples
pub fn compute_tokens_per_second_ema<I>(samples: I, window: EmaWindow) -> Option<f64>
where
    I: IntoIterator<Item = (i64, u128)>,
{
    let samples: Vec<_> = samples.into_iter().collect();
    if samples.len() < 2 {
        return None;
    }

    let mut state = EmaState::default();
    let mut last_value = None;

    for i in 1..samples.len() {
        let (prev_ts, prev_amount) = samples[i - 1];
        let (curr_ts, curr_amount) = samples[i];

        let delta_ms = curr_ts - prev_ts;
        if delta_ms <= 0 {
            continue;
        }

        let delta_amount = curr_amount.saturating_sub(prev_amount);
        let tokens_per_sec = (delta_amount as f64) / (delta_ms as f64 / 1000.0);

        state.update(tokens_per_sec, curr_ts, window);
        last_value = Some(state.value);
    }

    last_value
}

/// Thread-safe EMA tracker for real-time updates.
#[derive(Clone)]
pub struct EmaTracker {
    /// EMA states for each window type.
    states: Arc<RwLock<EmaTrackerState>>,
}

#[derive(Default)]
struct EmaTrackerState {
    s5: EmaState,
    m1: EmaState,
    m5: EmaState,
    m15: EmaState,
}

impl Default for EmaTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl EmaTracker {
    pub fn new() -> Self {
        Self {
            states: Arc::new(RwLock::new(EmaTrackerState::default())),
        }
    }

    /// Updates all EMA windows with a new value.
    pub async fn update(&self, value: f64, now_ms: i64) {
        let mut states = self.states.write().await;
        states.s5.update(value, now_ms, EmaWindow::S5);
        states.m1.update(value, now_ms, EmaWindow::M1);
        states.m5.update(value, now_ms, EmaWindow::M5);
        states.m15.update(value, now_ms, EmaWindow::M15);
    }

    /// Gets the current EMA value for a specific window.
    pub async fn get(&self, window: EmaWindow) -> f64 {
        let states = self.states.read().await;
        match window {
            EmaWindow::S5 => states.s5.value,
            EmaWindow::M1 => states.m1.value,
            EmaWindow::M5 => states.m5.value,
            EmaWindow::M15 => states.m15.value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ema_window_seconds() {
        assert_eq!(EmaWindow::S5.seconds(), 5);
        assert_eq!(EmaWindow::M1.seconds(), 60);
        assert_eq!(EmaWindow::M5.seconds(), 300);
        assert_eq!(EmaWindow::M15.seconds(), 900);
    }

    #[test]
    fn test_ema_alpha() {
        // alpha = 2 / (N + 1)
        assert!((EmaWindow::S5.alpha() - 2.0 / 6.0).abs() < 0.001);
        assert!((EmaWindow::M1.alpha() - 2.0 / 61.0).abs() < 0.001);
    }

    #[test]
    fn test_ema_computation() {
        let samples = vec![(1000, 10.0), (2000, 20.0), (3000, 30.0), (4000, 25.0)];

        let result = compute_ema_from_samples(samples, EmaWindow::S5);
        assert!(result.is_some());
        // EMA should be close to recent values but smoothed
        let ema = result.unwrap();
        assert!(ema > 10.0 && ema < 30.0);
    }

    #[test]
    fn test_tps_ema() {
        let samples = vec![(0, 0u64), (1000, 10), (2000, 25), (3000, 35)];

        let result = compute_tps_ema(samples, EmaWindow::S5);
        assert!(result.is_some());
        // TPS should be positive
        assert!(result.unwrap() > 0.0);
    }
}

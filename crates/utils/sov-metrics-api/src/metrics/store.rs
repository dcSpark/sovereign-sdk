use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::RwLock;

#[derive(Clone, Debug, Serialize)]
pub struct MetricSample {
    pub recorded_at_ms: i64,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricSeriesSnapshot {
    pub name: String,
    pub interval_secs: u64,
    pub max_samples: usize,
    pub latest: Option<MetricSample>,
    pub samples: Vec<MetricSample>,
}

#[derive(Clone, Debug)]
struct MetricSeries {
    interval_secs: u64,
    max_samples: usize,
    samples: VecDeque<MetricSample>,
}

#[derive(Clone, Debug)]
pub struct MetricsStore {
    inner: Arc<RwLock<HashMap<&'static str, MetricSeries>>>,
}

impl MetricsStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register_metric(&self, name: &'static str, interval_secs: u64, max_samples: usize) {
        let mut guard = self.inner.write().await;
        guard.entry(name).or_insert_with(|| MetricSeries {
            interval_secs,
            max_samples,
            samples: VecDeque::with_capacity(max_samples),
        });
    }

    pub async fn record(&self, name: &'static str, sample: MetricSample) -> bool {
        let mut guard = self.inner.write().await;
        match guard.get_mut(name) {
            Some(series) => {
                if series.samples.len() == series.max_samples {
                    series.samples.pop_front();
                }
                series.samples.push_back(sample);
                true
            }
            None => false,
        }
    }

    pub async fn snapshot(&self, name: &'static str) -> Option<MetricSeriesSnapshot> {
        let guard = self.inner.read().await;
        guard.get(name).map(|series| MetricSeriesSnapshot {
            name: name.to_string(),
            interval_secs: series.interval_secs,
            max_samples: series.max_samples,
            latest: series.samples.back().cloned(),
            samples: series.samples.iter().cloned().collect(),
        })
    }
}

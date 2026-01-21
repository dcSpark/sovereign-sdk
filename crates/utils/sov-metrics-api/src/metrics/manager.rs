use std::sync::Arc;

use tokio::time::interval;
use tracing::warn;

use crate::metrics::collector::{MetricCollector, MetricSpec};
use crate::metrics::store::MetricsStore;

struct RegisteredCollector {
    spec: MetricSpec,
    collector: Arc<dyn MetricCollector>,
}

pub struct MetricsManager {
    store: MetricsStore,
    collectors: Vec<RegisteredCollector>,
}

impl MetricsManager {
    pub fn new(store: MetricsStore) -> Self {
        Self {
            store,
            collectors: Vec::new(),
        }
    }

    pub async fn register<C: MetricCollector + 'static>(&mut self, collector: C) {
        let spec = collector.spec();
        self.store
            .register_metric(spec.name, spec.interval.as_secs())
            .await;
        self.collectors.push(RegisteredCollector {
            spec,
            collector: Arc::new(collector),
        });
    }

    pub fn start(self) {
        for registered in self.collectors {
            let store = self.store.clone();
            let spec = registered.spec.clone();
            let collector = registered.collector.clone();
            tokio::spawn(async move {
                let mut ticker = interval(spec.interval);
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    match collector.collect().await {
                        Ok(samples) => {
                            if samples.is_empty() {
                                continue;
                            }
                            if !store.record(spec.name, samples).await {
                                warn!(metric = spec.name, "Metric not registered");
                            }
                        }
                        Err(error) => {
                            warn!(metric = spec.name, error = %error, "Metric collection failed");
                        }
                    }
                }
            });
        }
    }
}

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;

use crate::metrics::store::MetricSample;

#[derive(Clone, Debug)]
pub struct MetricSpec {
    pub name: &'static str,
    pub interval: Duration,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait MetricCollector: Send + Sync {
    fn spec(&self) -> MetricSpec;
    fn collect<'a>(&'a self) -> BoxFuture<'a, Result<Vec<MetricSample>>>;
}

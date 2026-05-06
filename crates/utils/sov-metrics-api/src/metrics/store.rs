use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Map, Number, Value};
use tokio::sync::RwLock;
use tokio::task;
use tracing::warn;
use tsink::{DataPoint, Label, Row, Storage, StorageBuilder, TimestampPrecision};

#[derive(Clone, Debug, Serialize)]
pub struct MetricSample {
    pub recorded_at_ms: i64,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricSeriesSnapshot {
    pub name: String,
    pub interval_secs: u64,
    pub latest: Option<MetricSample>,
    pub samples: Vec<MetricSample>,
}

#[derive(Clone, Debug)]
struct MetricSeriesConfig {
    interval_secs: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricRecordResult {
    Recorded,
    NotRegistered,
    WriteFailed,
}

#[derive(Clone)]
pub struct MetricsStore {
    storage: Arc<dyn Storage>,
    inner: Arc<RwLock<HashMap<&'static str, MetricSeriesConfig>>>,
    retention_secs: u64,
}

impl MetricsStore {
    pub fn new(data_path: PathBuf, retention_secs: u64) -> Result<Self> {
        fs::create_dir_all(&data_path).with_context(|| {
            format!(
                "Failed to create tsink data path at {}",
                data_path.display()
            )
        })?;

        let storage = StorageBuilder::new()
            .with_data_path(&data_path)
            .with_timestamp_precision(TimestampPrecision::Milliseconds)
            .with_retention(Duration::from_secs(retention_secs))
            .build()
            .context("Failed to initialize tsink storage")?;

        Ok(Self {
            storage,
            inner: Arc::new(RwLock::new(HashMap::new())),
            retention_secs,
        })
    }

    pub async fn register_metric(&self, name: &'static str, interval_secs: u64) {
        let mut guard = self.inner.write().await;
        guard
            .entry(name)
            .or_insert(MetricSeriesConfig { interval_secs });
    }

    pub async fn record(
        &self,
        name: &'static str,
        samples: Vec<MetricSample>,
    ) -> MetricRecordResult {
        let registered = {
            let guard = self.inner.read().await;
            guard.contains_key(name)
        };
        if !registered {
            return MetricRecordResult::NotRegistered;
        }

        if samples.is_empty() {
            return MetricRecordResult::Recorded;
        }

        let mut rows = Vec::new();
        for sample in &samples {
            rows.extend(rows_from_sample(name, sample));
        }
        if rows.is_empty() {
            warn!(metric = name, "Metric payload produced no numeric fields");
            return MetricRecordResult::WriteFailed;
        }

        let storage = self.storage.clone();
        let result = task::spawn_blocking(move || storage.insert_rows(&rows)).await;
        match result {
            Ok(Ok(())) => MetricRecordResult::Recorded,
            Ok(Err(error)) => {
                warn!(metric = name, error = %error, "Failed to insert tsink rows");
                MetricRecordResult::WriteFailed
            }
            Err(error) => {
                warn!(metric = name, error = %error, "Failed to join tsink insert task");
                MetricRecordResult::WriteFailed
            }
        }
    }

    pub async fn values_in_range(
        &self,
        name: &'static str,
        from_ms: i64,
        to_ms: i64,
    ) -> Option<Vec<f64>> {
        let registered = {
            let guard = self.inner.read().await;
            guard.contains_key(name)
        };
        if !registered {
            return None;
        }

        let storage = self.storage.clone();
        let result = task::spawn_blocking(move || storage.select_all(name, from_ms, to_ms)).await;
        let series = match result {
            Ok(Ok(series)) => series,
            Ok(Err(error)) => {
                warn!(metric = name, error = %error, "Failed to query tsink data");
                return None;
            }
            Err(error) => {
                warn!(metric = name, error = %error, "Failed to join tsink query task");
                return None;
            }
        };

        let mut values = Vec::new();
        for (_, points) in series {
            for point in points {
                if point.value.is_finite() {
                    values.push(point.value);
                }
            }
        }

        Some(values)
    }

    pub async fn snapshot(&self, name: &'static str) -> Option<MetricSeriesSnapshot> {
        let config = {
            let guard = self.inner.read().await;
            guard.get(name).cloned()
        }?;

        let end_ms = chrono::Utc::now().timestamp_millis();
        let retention_ms = self.retention_secs.saturating_mul(1000);
        let retention_ms = match i64::try_from(retention_ms) {
            Ok(value) => value,
            Err(_) => {
                warn!(metric = name, "Retention window overflow, using full range");
                i64::MAX
            }
        };
        let start_ms = end_ms.saturating_sub(retention_ms);

        self.snapshot_with_range(name, config, start_ms, end_ms)
            .await
    }

    pub async fn snapshot_range(
        &self,
        name: &'static str,
        start_ms: i64,
        end_ms: i64,
    ) -> Option<MetricSeriesSnapshot> {
        if start_ms >= end_ms || start_ms < 0 || end_ms < 0 {
            warn!(metric = name, start_ms, end_ms, "Invalid snapshot range");
            return None;
        }

        let config = {
            let guard = self.inner.read().await;
            guard.get(name).cloned()
        }?;

        self.snapshot_with_range(name, config, start_ms, end_ms)
            .await
    }

    async fn snapshot_with_range(
        &self,
        name: &'static str,
        config: MetricSeriesConfig,
        start_ms: i64,
        end_ms: i64,
    ) -> Option<MetricSeriesSnapshot> {
        let storage = self.storage.clone();
        let result = task::spawn_blocking(move || storage.select_all(name, start_ms, end_ms)).await;
        let series = match result {
            Ok(Ok(series)) => series,
            Ok(Err(error)) => {
                warn!(metric = name, error = %error, "Failed to query tsink data");
                return None;
            }
            Err(error) => {
                warn!(metric = name, error = %error, "Failed to join tsink query task");
                return None;
            }
        };

        let mut buckets: BTreeMap<i64, Map<String, Value>> = BTreeMap::new();
        for (labels, points) in series {
            let (field, kind) = match field_from_labels(&labels) {
                Some(field) => field,
                None => continue,
            };
            for point in points {
                let value = match value_from_point(&point, kind) {
                    Some(value) => value,
                    None => continue,
                };
                buckets
                    .entry(point.timestamp)
                    .or_insert_with(Map::new)
                    .insert(field.clone(), value);
            }
        }

        let samples: Vec<MetricSample> = buckets
            .into_iter()
            .map(|(timestamp, payload)| MetricSample {
                recorded_at_ms: timestamp,
                payload: Value::Object(payload),
            })
            .collect();

        let latest = samples.last().cloned();

        Some(MetricSeriesSnapshot {
            name: name.to_string(),
            interval_secs: config.interval_secs,
            latest,
            samples,
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum FieldKind {
    U64,
    F64,
    String,
}

impl FieldKind {
    fn label_value(self) -> &'static str {
        match self {
            FieldKind::U64 => "u64",
            FieldKind::F64 => "f64",
            FieldKind::String => "string",
        }
    }

    fn from_label(value: &str) -> Option<Self> {
        match value {
            "u64" => Some(FieldKind::U64),
            "f64" => Some(FieldKind::F64),
            "string" => Some(FieldKind::String),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct FieldPoint {
    field: String,
    value: f64,
    kind: FieldKind,
}

fn rows_from_sample(name: &'static str, sample: &MetricSample) -> Vec<Row> {
    let mut points = Vec::new();
    flatten_payload(&sample.payload, None, &mut points);

    points
        .into_iter()
        .map(|point| {
            let labels = vec![
                Label::new("field", point.field),
                Label::new("kind", point.kind.label_value()),
            ];
            Row::with_labels(
                name.to_string(),
                labels,
                DataPoint::new(sample.recorded_at_ms, point.value),
            )
        })
        .collect()
}

fn flatten_payload(value: &Value, prefix: Option<&str>, out: &mut Vec<FieldPoint>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let path = match prefix {
                    Some(prefix) => format!("{prefix}.{key}"),
                    None => key.clone(),
                };
                flatten_payload(value, Some(&path), out);
            }
        }
        Value::Array(values) => {
            for (idx, value) in values.iter().enumerate() {
                let path = match prefix {
                    Some(prefix) => format!("{prefix}.{idx}"),
                    None => idx.to_string(),
                };
                flatten_payload(value, Some(&path), out);
            }
        }
        Value::Number(number) => {
            let field = prefix.unwrap_or("value").to_string();
            if let Some(value) = number.as_u64() {
                out.push(FieldPoint {
                    field,
                    value: value as f64,
                    kind: FieldKind::U64,
                });
            } else if let Some(value) = number.as_i64() {
                out.push(FieldPoint {
                    field,
                    value: value as f64,
                    kind: FieldKind::F64,
                });
            } else if let Some(value) = number.as_f64() {
                out.push(FieldPoint {
                    field,
                    value,
                    kind: FieldKind::F64,
                });
            }
        }
        Value::String(value) => {
            let field = prefix.unwrap_or("value").to_string();
            match value.parse::<f64>() {
                Ok(parsed) => out.push(FieldPoint {
                    field,
                    value: parsed,
                    kind: FieldKind::String,
                }),
                Err(error) => {
                    warn!(field, value, error = %error, "Skipping non-numeric string payload");
                }
            }
        }
        Value::Bool(value) => {
            let field = prefix.unwrap_or("value").to_string();
            out.push(FieldPoint {
                field,
                value: if *value { 1.0 } else { 0.0 },
                kind: FieldKind::U64,
            });
        }
        Value::Null => {}
    }
}

fn field_from_labels(labels: &[Label]) -> Option<(String, FieldKind)> {
    let mut field = None;
    let mut kind = None;

    for label in labels {
        if label.name == "field" {
            field = Some(label.value.clone());
        } else if label.name == "kind" {
            kind = FieldKind::from_label(&label.value);
        }
    }

    let field = field?;
    let kind = kind.unwrap_or(FieldKind::F64);

    Some((field, kind))
}

fn value_from_point(point: &DataPoint, kind: FieldKind) -> Option<Value> {
    match kind {
        FieldKind::U64 => {
            let value = f64_to_u64(point.value)?;
            Some(Value::Number(Number::from(value)))
        }
        FieldKind::F64 => Number::from_f64(point.value)
            .map(Value::Number)
            .or_else(|| {
                warn!(value = point.value, "Failed to encode f64 data point");
                None
            }),
        FieldKind::String => Some(Value::String(format!("{:.0}", point.value))),
    }
}

fn f64_to_u64(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 || value > u64::MAX as f64 {
        warn!(value, "Unable to convert f64 to u64");
        return None;
    }

    let rounded = value.round();
    if (value - rounded).abs() > f64::EPSILON {
        warn!(value, "Non-integer value stored as u64");
    }
    Some(rounded as u64)
}

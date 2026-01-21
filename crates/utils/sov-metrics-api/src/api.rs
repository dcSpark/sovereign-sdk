use axum::{
    extract::{Query, Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tracing::warn;
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

use crate::metrics::collectors::average_transaction_size::AverageTransactionSizePayload;
use crate::metrics::collectors::failed_transactions::{
    FailedTransactionsPayload, RETENTION_SECONDS as FAILED_RETENTION_SECONDS,
};
use crate::metrics::collectors::token_value_spent::{
    TokenValueSpentPayload, RETENTION_SECONDS as TOKEN_VALUE_RETENTION_SECONDS,
};
use crate::metrics::collectors::total_tokens_economy::TotalTokensEconomyPayload;
use crate::metrics::collectors::total_transactions::TotalTransactionsPayload;
use crate::metrics::{MetricSample, MetricSeriesSnapshot, MetricsStore};

#[derive(Clone)]
pub struct AppState {
    pub store: MetricsStore,
}

#[derive(Debug, Deserialize)]
struct WindowQuery {
    window_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenVelocityQuery {
    window_seconds: Option<u64>,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
}

pub fn router(state: AppState) -> Router {
    let swagger_ui = Router::from(
        SwaggerUi::new("/swagger-ui").url("/api-doc/openapi.json", ApiDoc::openapi()),
    )
    .layer(middleware::from_fn(swagger_ui_redirect));

    Router::new()
        .route("/health", get(health))
        .route("/average-transaction-size", get(average_transaction_size))
        .route("/failed-transactions-rate", get(failed_transactions_rate))
        .route("/median-transaction-size", get(median_transaction_size))
        .route("/token-value-spent", get(token_value_spent))
        .route("/token-velocity", get(token_velocity))
        .route("/total-transactions", get(total_transactions))
        .route("/tps", get(tps))
        .merge(swagger_ui)
        .with_state(state)
}

async fn swagger_ui_redirect(req: Request, next: Next) -> Response {
    if req.uri().path() == "/swagger-ui" {
        return Redirect::permanent("/swagger-ui/").into_response();
    }

    next.run(req).await
}

#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "Service health", body = HealthResponse)
    ),
    tag = "health"
)]
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
    })
}

#[utoipa::path(
    get,
    path = "/total-transactions",
    responses(
        (status = 200, description = "Cumulative completed transaction totals", body = TotalTransactionsResponse)
    ),
    tag = "metrics"
)]
async fn total_transactions(State(state): State<AppState>) -> Json<TotalTransactionsResponse> {
    let latest = state
        .store
        .snapshot("total-transactions")
        .await
        .and_then(|series| series.latest.or_else(|| series.samples.last().cloned()));

    let (total_transactions, as_of_ms) = match latest {
        Some(sample) => match decode_payload::<TotalTransactionsPayload>(&sample, "total-transactions") {
            Some(payload) => (Some(payload.total_transactions), Some(sample.recorded_at_ms)),
            None => (None, None),
        },
        None => (None, None),
    };

    Json(TotalTransactionsResponse {
        total_transactions,
        as_of_ms,
    })
}

#[utoipa::path(
    get,
    path = "/tps",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Window size in seconds used to compute TPS. Defaults to the last two samples.")
    ),
    responses(
        (status = 200, description = "Derived TPS from transaction counters", body = TpsResponse)
    ),
    tag = "metrics"
)]
async fn tps(
    State(state): State<AppState>,
    Query(params): Query<WindowQuery>,
) -> Json<TpsResponse> {
    let series = state
        .store
        .snapshot("total-transactions")
        .await
        .map(map_total_transactions_series);

    let window_ms = window_ms(params.window_seconds);
    let (tps, delta_transactions, delta_ms, latest_total) = match series.as_ref() {
        Some(series) => compute_tps(series, window_ms),
        None => (None, None, None, None),
    };

    Json(TpsResponse {
        tps,
        delta_transactions,
        delta_ms,
        latest_total,
    })
}

#[utoipa::path(
    get,
    path = "/failed-transactions-rate",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Window size in seconds used to compute the failure rate. Defaults to the last two samples.")
    ),
    responses(
        (status = 200, description = "Rejected transaction rate", body = FailedTransactionsResponse)
    ),
    tag = "metrics"
)]
async fn failed_transactions_rate(
    State(state): State<AppState>,
    Query(params): Query<WindowQuery>,
) -> Json<FailedTransactionsResponse> {
    let series_snapshot = state
        .store
        .snapshot("failed-transactions-rate")
        .await
        .map(map_failed_transactions_series);

    let window_ms = window_ms(params.window_seconds);
    let (rate_percent, failed_transactions, total_transactions, delta_ms) =
        match series_snapshot.as_ref() {
        Some(series) => compute_failed_rate(series, window_ms),
        None => (None, None, None, None),
    };

    Json(FailedTransactionsResponse {
        rate_percent,
        failed_transactions,
        total_transactions,
        delta_ms,
        retention_seconds: FAILED_RETENTION_SECONDS,
    })
}

#[utoipa::path(
    get,
    path = "/average-transaction-size",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Window size in seconds used to compute the average amount. Defaults to 24 hours.")
    ),
    responses(
        (status = 200, description = "Average transfer amount", body = AverageTransactionSizeResponse)
    ),
    tag = "metrics"
)]
async fn average_transaction_size(
    State(state): State<AppState>,
    Query(params): Query<WindowQuery>,
) -> Json<AverageTransactionSizeResponse> {
    let series_snapshot = state
        .store
        .snapshot("token-value-spent")
        .await
        .map(map_average_transaction_size_series);

    let window_ms = match params.window_seconds {
        Some(window_seconds) => window_ms(Some(window_seconds)),
        None => window_ms(Some(86_400)),
    };
    let (average_amount, delta_amount, delta_transactions, delta_ms) =
        match series_snapshot.as_ref() {
        Some(series) => compute_average_transaction_size(series, window_ms),
        None => (None, None, None, None),
    };

    Json(AverageTransactionSizeResponse {
        average_amount,
        delta_amount,
        delta_transactions,
        delta_ms,
        retention_seconds: TOKEN_VALUE_RETENTION_SECONDS,
    })
}

#[utoipa::path(
    get,
    path = "/median-transaction-size",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Window size in seconds used to compute the median amount. Defaults to 24 hours.")
    ),
    responses(
        (status = 200, description = "Median transaction size", body = MedianTransactionSizeResponse)
    ),
    tag = "metrics"
)]
async fn median_transaction_size(
    State(state): State<AppState>,
    Query(params): Query<WindowQuery>,
) -> Json<MedianTransactionSizeResponse> {
    let window_ms = match params.window_seconds {
        Some(window_seconds) => window_ms(Some(window_seconds)),
        None => window_ms(Some(86_400)),
    };

    let now_ms = chrono::Utc::now().timestamp_millis();
    let start_ms = window_ms
        .and_then(|window_ms| now_ms.checked_sub(window_ms))
        .unwrap_or(now_ms);

    let mut values = state
        .store
        .values_in_range("transaction-size", start_ms, now_ms)
        .await
        .unwrap_or_default();

    values.retain(|value| value.is_finite());
    if values.is_empty() {
        return Json(MedianTransactionSizeResponse { median_amount: None });
    }

    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    let median = if values.len() % 2 == 0 {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    };

    Json(MedianTransactionSizeResponse {
        median_amount: Some(median),
    })
}

#[utoipa::path(
    get,
    path = "/token-value-spent",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Window size in seconds used to compute total tokens transferred. Defaults to 24 hours.")
    ),
    responses(
        (status = 200, description = "Token value spent", body = TokenValueSpentResponse)
    ),
    tag = "metrics"
)]
async fn token_value_spent(
    State(state): State<AppState>,
    Query(params): Query<WindowQuery>,
) -> Json<TokenValueSpentResponse> {
    let series = state.store.snapshot("token-value-spent").await;
    let window_ms = match params.window_seconds {
        Some(window_seconds) => window_ms(Some(window_seconds)),
        None => window_ms(Some(86_400)),
    };
    let value_spent = series
        .as_ref()
        .and_then(|series| compute_token_value_spent(series, window_ms));

    Json(TokenValueSpentResponse {
        value_spent,
    })
}

#[utoipa::path(
    get,
    path = "/token-velocity",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Window size in seconds used to compute token velocity. Defaults to 24 hours."),
        ("from_ms" = Option<i64>, Query, description = "Start of the range (milliseconds since epoch). Requires `to_ms`."),
        ("to_ms" = Option<i64>, Query, description = "End of the range (milliseconds since epoch). Requires `from_ms`.")
    ),
    responses(
        (status = 200, description = "Token velocity", body = TokenVelocityResponse)
    ),
    tag = "metrics"
)]
async fn token_velocity(
    State(state): State<AppState>,
    Query(params): Query<TokenVelocityQuery>,
) -> Json<TokenVelocityResponse> {
    let range = match (params.from_ms, params.to_ms) {
        (Some(from_ms), Some(to_ms)) => {
            if from_ms >= to_ms {
                warn!(from_ms, to_ms, "Invalid token velocity range");
                None
            } else if from_ms < 0 || to_ms < 0 {
                warn!(from_ms, to_ms, "Negative token velocity range not supported");
                None
            } else {
                Some((from_ms, to_ms))
            }
        }
        _ => None,
    };

    let value_spent = state
        .store
        .snapshot("token-value-spent")
        .await
        .and_then(|series| match range {
            Some((from_ms, to_ms)) => compute_token_value_spent_range(&series, from_ms, to_ms),
            None => {
                let window_ms = match params.window_seconds {
                    Some(window_seconds) => window_ms(Some(window_seconds)),
                    None => window_ms(Some(86_400)),
                };
                compute_token_value_spent(&series, window_ms)
            }
        });

    let total_tokens = state
        .store
        .snapshot("total-tokens-economy")
        .await
        .and_then(|series| match range {
            Some((from_ms, to_ms)) => compute_total_tokens_average_range(&series, from_ms, to_ms),
            None => {
                let window_ms = match params.window_seconds {
                    Some(window_seconds) => window_ms(Some(window_seconds)),
                    None => window_ms(Some(86_400)),
                };
                compute_total_tokens_average_window(&series, window_ms)
            }
        });

    let token_velocity = match (
        value_spent.as_ref().and_then(|value| parse_amount(value)),
        total_tokens.as_ref().and_then(|value| parse_amount(value)),
    ) {
        (Some(spent), Some(total)) => {
            if total == 0 {
                None
            } else {
                Some(spent as f64 / total as f64)
            }
        }
        _ => None,
    };

    Json(TokenVelocityResponse {
        token_velocity,
        value_spent,
        total_tokens,
    })
}

fn decode_payload<T: DeserializeOwned>(sample: &MetricSample, label: &str) -> Option<T> {
    match serde_json::from_value(sample.payload.clone()) {
        Ok(payload) => Some(payload),
        Err(error) => {
            warn!(error = %error, label, "Failed to parse metric payload");
            None
        }
    }
}

struct WindowedSamples<'a, T> {
    start: &'a T,
    latest: &'a T,
    delta_ms: i64,
}

fn window_ms(window_seconds: Option<u64>) -> Option<i64> {
    let seconds = window_seconds?;
    if seconds == 0 {
        return None;
    }
    let millis = seconds.saturating_mul(1000);
    match i64::try_from(millis) {
        Ok(ms) => Some(ms),
        Err(_) => {
            warn!(seconds, "window_seconds too large, using default window");
            None
        }
    }
}

fn select_window<'a, T>(
    samples: &'a [T],
    window_ms: Option<i64>,
    timestamp: impl Fn(&T) -> i64,
) -> Option<WindowedSamples<'a, T>> {
    if samples.len() < 2 {
        return None;
    }

    let latest_idx = samples.len() - 1;
    let latest = &samples[latest_idx];
    let latest_ts = timestamp(latest);

    let start_idx = match window_ms {
        Some(window_ms) if window_ms > 0 => {
            let target = latest_ts - window_ms;
            let idx = samples
                .iter()
                .rposition(|sample| timestamp(sample) <= target)
                .unwrap_or(0);
            if idx == latest_idx {
                latest_idx - 1
            } else {
                idx
            }
        }
        _ => latest_idx - 1,
    };

    let start = &samples[start_idx];
    let delta_ms = latest_ts - timestamp(start);
    if delta_ms <= 0 {
        return None;
    }

    Some(WindowedSamples {
        start,
        latest,
        delta_ms,
    })
}

fn compute_tps(
    series: &TotalTransactionsSeriesSnapshot,
    window_ms: Option<i64>,
) -> (Option<f64>, Option<u64>, Option<i64>, Option<u64>) {
    let latest_total = series
        .samples
        .last()
        .map(|sample| sample.payload.total_transactions);
    let window = match select_window(&series.samples, window_ms, |sample| sample.recorded_at_ms) {
        Some(window) => window,
        None => return (None, None, None, latest_total),
    };

    let delta_transactions = window
        .latest
        .payload
        .total_transactions
        .saturating_sub(window.start.payload.total_transactions);
    let delta_ms = window.delta_ms;
    if delta_ms <= 0 {
        return (
            None,
            Some(delta_transactions),
            Some(delta_ms),
            latest_total,
        );
    }

    let tps = (delta_transactions as f64) / (delta_ms as f64 / 1000.0);

    (
        Some(tps),
        Some(delta_transactions),
        Some(delta_ms),
        latest_total,
    )
}

fn compute_failed_rate(
    series: &FailedTransactionsSeriesSnapshot,
    window_ms: Option<i64>,
) -> (Option<f64>, Option<u64>, Option<u64>, Option<i64>) {
    let window = match select_window(&series.samples, window_ms, |sample| sample.recorded_at_ms) {
        Some(window) => window,
        None => return (None, None, None, None),
    };

    let delta_total = window
        .latest
        .payload
        .total_transactions
        .saturating_sub(window.start.payload.total_transactions);
    let delta_failed = window
        .latest
        .payload
        .failed_transactions
        .saturating_sub(window.start.payload.failed_transactions);

    if delta_total == 0 {
        return (
            None,
            Some(delta_failed),
            Some(delta_total),
            Some(window.delta_ms),
        );
    }

    let rate_percent = (delta_failed as f64 / delta_total as f64) * 100.0;

    (
        Some(rate_percent),
        Some(delta_failed),
        Some(delta_total),
        Some(window.delta_ms),
    )
}

fn compute_average_transaction_size(
    series: &AverageTransactionSizeSeriesSnapshot,
    window_ms: Option<i64>,
) -> (Option<f64>, Option<String>, Option<u64>, Option<i64>) {
    let window = match select_window(&series.samples, window_ms, |sample| sample.recorded_at_ms) {
        Some(window) => window,
        None => return (None, None, None, None),
    };

    let prev_total = match parse_amount(&window.start.payload.total_amount) {
        Some(total) => total,
        None => return (None, None, None, None),
    };
    let latest_total = match parse_amount(&window.latest.payload.total_amount) {
        Some(total) => total,
        None => return (None, None, None, None),
    };
    let delta_amount = latest_total.saturating_sub(prev_total);
    let delta_transactions = window
        .latest
        .payload
        .total_transactions
        .saturating_sub(window.start.payload.total_transactions);

    if delta_transactions == 0 {
        return (
            None,
            Some(delta_amount.to_string()),
            Some(delta_transactions),
            Some(window.delta_ms),
        );
    }

    let average_amount = (delta_amount as f64) / (delta_transactions as f64);

    (
        Some(average_amount),
        Some(delta_amount.to_string()),
        Some(delta_transactions),
        Some(window.delta_ms),
    )
}

fn compute_token_value_spent(
    series: &MetricSeriesSnapshot,
    window_ms: Option<i64>,
) -> Option<String> {
    let window = select_window(&series.samples, window_ms, |sample| sample.recorded_at_ms)?;

    let prev_payload: TokenValueSpentPayload =
        decode_payload(window.start, "token-value-spent")?;
    let latest_payload: TokenValueSpentPayload =
        decode_payload(window.latest, "token-value-spent")?;
    let prev_total = parse_amount(&prev_payload.total_amount)?;
    let latest_total = parse_amount(&latest_payload.total_amount)?;
    let delta_amount = latest_total.saturating_sub(prev_total);

    Some(delta_amount.to_string())
}

fn parse_amount(value: &str) -> Option<u128> {
    match value.parse::<u128>() {
        Ok(parsed) => Some(parsed),
        Err(error) => {
            warn!(error = %error, value, "Failed to parse amount");
            None
        }
    }
}


fn select_range<'a, T>(
    samples: &'a [T],
    from_ms: i64,
    to_ms: i64,
    timestamp: impl Fn(&T) -> i64,
) -> Option<WindowedSamples<'a, T>> {
    if samples.len() < 2 {
        return None;
    }

    if from_ms >= to_ms {
        return None;
    }

    let end_idx = samples
        .iter()
        .rposition(|sample| timestamp(sample) <= to_ms)?;
    let mut start_idx = samples
        .iter()
        .rposition(|sample| timestamp(sample) <= from_ms)
        .unwrap_or(0);

    if start_idx >= end_idx {
        if end_idx == 0 {
            return None;
        }
        start_idx = end_idx - 1;
    }

    let start = &samples[start_idx];
    let latest = &samples[end_idx];
    let delta_ms = timestamp(latest) - timestamp(start);
    if delta_ms <= 0 {
        return None;
    }

    Some(WindowedSamples {
        start,
        latest,
        delta_ms,
    })
}

fn compute_token_value_spent_range(
    series: &MetricSeriesSnapshot,
    from_ms: i64,
    to_ms: i64,
) -> Option<String> {
    let window = select_range(&series.samples, from_ms, to_ms, |sample| sample.recorded_at_ms)?;
    let prev_payload: TokenValueSpentPayload =
        decode_payload(window.start, "token-value-spent")?;
    let latest_payload: TokenValueSpentPayload =
        decode_payload(window.latest, "token-value-spent")?;
    let prev_total = parse_amount(&prev_payload.total_amount)?;
    let latest_total = parse_amount(&latest_payload.total_amount)?;
    let delta_amount = latest_total.saturating_sub(prev_total);

    Some(delta_amount.to_string())
}

fn compute_total_tokens_average_window(
    series: &MetricSeriesSnapshot,
    window_ms: Option<i64>,
) -> Option<String> {
    let window = select_window(&series.samples, window_ms, |sample| sample.recorded_at_ms)?;
    total_tokens_average_from_samples(window.start, window.latest)
}

fn compute_total_tokens_average_range(
    series: &MetricSeriesSnapshot,
    from_ms: i64,
    to_ms: i64,
) -> Option<String> {
    let window = select_range(&series.samples, from_ms, to_ms, |sample| sample.recorded_at_ms)?;
    total_tokens_average_from_samples(window.start, window.latest)
}

fn total_tokens_average_from_samples(
    start: &MetricSample,
    latest: &MetricSample,
) -> Option<String> {
    let start_payload: TotalTokensEconomyPayload = decode_payload(start, "total-tokens-economy")?;
    let latest_payload: TotalTokensEconomyPayload =
        decode_payload(latest, "total-tokens-economy")?;
    let start_total = parse_amount(&start_payload.total_amount)?;
    let latest_total = parse_amount(&latest_payload.total_amount)?;
    let sum = match start_total.checked_add(latest_total) {
        Some(sum) => sum,
        None => {
            warn!("Total token supply overflow while averaging");
            return None;
        }
    };
    let average = sum / 2;

    Some(average.to_string())
}

fn map_total_transactions_series(series: MetricSeriesSnapshot) -> TotalTransactionsSeriesSnapshot {
    let samples: Vec<TotalTransactionsSample> = series
        .samples
        .into_iter()
        .filter_map(map_total_transactions_sample)
        .collect();
    let latest = series.latest.and_then(map_total_transactions_sample);

    TotalTransactionsSeriesSnapshot {
        name: series.name,
        interval_secs: series.interval_secs,
        max_samples: series.max_samples,
        latest,
        samples,
    }
}

fn map_failed_transactions_series(series: MetricSeriesSnapshot) -> FailedTransactionsSeriesSnapshot {
    let samples: Vec<FailedTransactionsSample> = series
        .samples
        .into_iter()
        .filter_map(map_failed_transactions_sample)
        .collect();
    let latest = series.latest.and_then(map_failed_transactions_sample);

    FailedTransactionsSeriesSnapshot {
        name: series.name,
        interval_secs: series.interval_secs,
        max_samples: series.max_samples,
        latest,
        samples,
    }
}

fn map_average_transaction_size_series(
    series: MetricSeriesSnapshot,
) -> AverageTransactionSizeSeriesSnapshot {
    let samples: Vec<AverageTransactionSizeSample> = series
        .samples
        .into_iter()
        .filter_map(map_average_transaction_size_sample)
        .collect();
    let latest = series.latest.and_then(map_average_transaction_size_sample);

    AverageTransactionSizeSeriesSnapshot {
        name: "average-transaction-size".to_string(),
        interval_secs: series.interval_secs,
        max_samples: series.max_samples,
        latest,
        samples,
    }
}


fn map_total_transactions_sample(sample: MetricSample) -> Option<TotalTransactionsSample> {
    let payload: TotalTransactionsPayload = decode_payload(&sample, "total-transactions")?;

    Some(TotalTransactionsSample {
        recorded_at_ms: sample.recorded_at_ms,
        payload,
    })
}

fn map_failed_transactions_sample(sample: MetricSample) -> Option<FailedTransactionsSample> {
    let payload: FailedTransactionsPayload =
        decode_payload(&sample, "failed-transactions-rate")?;

    Some(FailedTransactionsSample {
        recorded_at_ms: sample.recorded_at_ms,
        payload,
    })
}

fn map_average_transaction_size_sample(
    sample: MetricSample,
) -> Option<AverageTransactionSizeSample> {
    let payload: AverageTransactionSizePayload =
        decode_payload(&sample, "average-transaction-size")?;

    Some(AverageTransactionSizeSample {
        recorded_at_ms: sample.recorded_at_ms,
        payload,
    })
}


#[derive(Serialize, ToSchema)]
struct HealthResponse {
    status: String,
}

#[derive(Serialize, ToSchema)]
struct TotalTransactionsResponse {
    total_transactions: Option<u64>,
    as_of_ms: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct TotalTransactionsSeriesSnapshot {
    name: String,
    interval_secs: u64,
    max_samples: usize,
    latest: Option<TotalTransactionsSample>,
    samples: Vec<TotalTransactionsSample>,
}

#[derive(Serialize, ToSchema)]
struct TotalTransactionsSample {
    recorded_at_ms: i64,
    payload: TotalTransactionsPayload,
}

#[derive(Serialize, ToSchema)]
struct TpsResponse {
    tps: Option<f64>,
    delta_transactions: Option<u64>,
    delta_ms: Option<i64>,
    latest_total: Option<u64>,
}

#[derive(Serialize, ToSchema)]
struct FailedTransactionsResponse {
    rate_percent: Option<f64>,
    failed_transactions: Option<u64>,
    total_transactions: Option<u64>,
    delta_ms: Option<i64>,
    retention_seconds: u64,
}

#[derive(Serialize, ToSchema)]
struct FailedTransactionsSeriesSnapshot {
    name: String,
    interval_secs: u64,
    max_samples: usize,
    latest: Option<FailedTransactionsSample>,
    samples: Vec<FailedTransactionsSample>,
}

#[derive(Serialize, ToSchema)]
struct FailedTransactionsSample {
    recorded_at_ms: i64,
    payload: FailedTransactionsPayload,
}

#[derive(Serialize, ToSchema)]
struct AverageTransactionSizeResponse {
    average_amount: Option<f64>,
    delta_amount: Option<String>,
    delta_transactions: Option<u64>,
    delta_ms: Option<i64>,
    retention_seconds: u64,
}

#[derive(Serialize, ToSchema)]
struct AverageTransactionSizeSeriesSnapshot {
    name: String,
    interval_secs: u64,
    max_samples: usize,
    latest: Option<AverageTransactionSizeSample>,
    samples: Vec<AverageTransactionSizeSample>,
}

#[derive(Serialize, ToSchema)]
struct AverageTransactionSizeSample {
    recorded_at_ms: i64,
    payload: AverageTransactionSizePayload,
}

#[derive(Serialize, ToSchema)]
struct MedianTransactionSizeResponse {
    median_amount: Option<f64>,
}

#[derive(Serialize, ToSchema)]
struct TokenValueSpentResponse {
    value_spent: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct TokenVelocityResponse {
    token_velocity: Option<f64>,
    value_spent: Option<String>,
    total_tokens: Option<String>,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Sovereign Metrics API",
        version = "0.1.0",
        description = "Metrics API for verifier worker DB stats."
    ),
    paths(
        health,
        total_transactions,
        tps,
        failed_transactions_rate,
        average_transaction_size,
        median_transaction_size,
        token_value_spent,
        token_velocity
    ),
    components(schemas(
        HealthResponse,
        TotalTransactionsResponse,
        TpsResponse,
        FailedTransactionsResponse,
        AverageTransactionSizeResponse,
        MedianTransactionSizeResponse,
        TokenValueSpentResponse,
        TokenVelocityResponse
    )),
    tags(
        (name = "health", description = "Service health checks"),
        (name = "metrics", description = "Metrics endpoints")
    )
)]
struct ApiDoc;

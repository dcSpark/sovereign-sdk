use axum::{
    extract::{Query, Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use sea_orm::DatabaseConnection;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::warn;
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

use crate::metrics::collectors::accounts::AccountsPayload;
use crate::metrics::collectors::average_transaction_size::AverageTransactionSizePayload;
use crate::metrics::collectors::failed_transactions::FailedTransactionsPayload;
use crate::metrics::collectors::token_value_spent::TokenValueSpentPayload;
use crate::metrics::collectors::total_tokens_economy::TotalTokensEconomyPayload;
use crate::metrics::collectors::total_transactions::TotalTransactionsPayload;
use crate::metrics::collectors::transaction_size::TransactionSizePayload;
use crate::metrics::{
    compute_ema_from_samples, compute_tokens_per_second_ema, EmaWindow, MetricSample,
    MetricSeriesSnapshot, MetricsStore,
};

/// Cached peak TPS value with its computation boundary.
#[derive(Clone, Debug)]
struct TpsPeakCacheEntry {
    /// The maximum TPS value found in the window.
    peak_tps: f64,
    /// Timestamp (ms) when the peak occurred.
    peak_at_ms: i64,
    /// Requested window size used for this computation.
    window_ms: i64,
    /// When this cache entry was computed (ms).
    computed_at_ms: i64,
}

/// Thread-safe cache for peak TPS calculations.
#[derive(Clone, Default)]
pub struct TpsPeakCache {
    inner: Arc<RwLock<Option<TpsPeakCacheEntry>>>,
}

impl TpsPeakCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub store: MetricsStore,
    pub retention_secs: u64,
    pub tps_peak_cache: TpsPeakCache,
    pub indexer_db: DatabaseConnection,
    /// Multiplier applied to PeakTPS metric output.
    pub peak_tps_multiplier: f64,
    /// Base URL for the rollup ledger API, used to query slot TPS.
    pub ledger_api_base_url: String,
    /// Shared HTTP client for ledger API requests.
    pub ledger_http_client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct WindowQuery {
    window_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TpsPeakQuery {
    window_seconds: Option<u64>,
    /// Set to true to bypass cache and force fresh calculation.
    no_cache: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct TokenVelocityQuery {
    window_seconds: Option<u64>,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct HistoricRangeQuery {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct HistoricWindowQuery {
    window_seconds: Option<u64>,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
}

const POSTGRES_AVERAGE_TRANSACTION_SIZE_SQL: &str = r#"
WITH windowed AS (
    SELECT
        mt.event_id,
        CAST(mt.amount AS numeric) AS amount
    FROM midnight_transfer mt
    INNER JOIN events ev ON ev.id = mt.event_id
    WHERE mt.amount IS NOT NULL
      AND mt.amount ~ '^[0-9]+$'
      AND ev.created_at >= $1
      AND ev.created_at <= $2
)
SELECT
    AVG(windowed.amount)::double precision AS average_amount,
    COALESCE(SUM(windowed.amount), 0)::text AS delta_amount,
    COUNT(*)::bigint AS delta_transactions
FROM windowed
"#;

const POSTGRES_MEDIAN_TRANSACTION_SIZE_SQL: &str = r#"
WITH windowed AS (
    SELECT
        mt.event_id,
        CAST(mt.amount AS numeric) AS amount
    FROM midnight_transfer mt
    INNER JOIN events ev ON ev.id = mt.event_id
    WHERE mt.amount IS NOT NULL
      AND mt.amount ~ '^[0-9]+$'
      AND ev.created_at >= $1
      AND ev.created_at <= $2
)
SELECT
    percentile_cont(0.5) WITHIN GROUP (ORDER BY windowed.amount)::double precision AS median_amount
FROM windowed
"#;

/// Maximum number of slots to scan when reconstructing TPS windows from /ledger/tps.
const MAX_LEDGER_TPS_SLOTS: usize = 5_000;
/// Default lookback for /tps/historic when no explicit range is provided.
const DEFAULT_TPS_HISTORIC_LOOKBACK_SECONDS: i64 = 300;

pub fn router(state: AppState) -> Router {
    let swagger_ui =
        Router::from(SwaggerUi::new("/swagger-ui").url("/api-doc/openapi.json", ApiDoc::openapi()))
            .layer(middleware::from_fn(swagger_ui_redirect));

    Router::new()
        .route("/health", get(health))
        .route("/average-transaction-size", get(average_transaction_size))
        .route(
            "/average-transaction-size/historic",
            get(average_transaction_size_historic),
        )
        .route("/failed-transactions-rate", get(failed_transactions_rate))
        .route(
            "/failed-transactions-rate/historic",
            get(failed_transactions_rate_historic),
        )
        .route("/median-transaction-size", get(median_transaction_size))
        .route(
            "/median-transaction-size/historic",
            get(median_transaction_size_historic),
        )
        // EMA metrics endpoints (MockMCP-compatible) - exposed via proxy at /metrics/*
        .route("/s2", get(metrics_s2))
        .route("/s5", get(metrics_s5))
        .route("/m1", get(metrics_m1))
        .route("/m5", get(metrics_m5))
        .route("/m15", get(metrics_m15))
        .route("/token-value-spent", get(token_value_spent))
        .route(
            "/token-value-spent/historic",
            get(token_value_spent_historic),
        )
        .route("/token-velocity", get(token_velocity))
        .route("/token-velocity/historic", get(token_velocity_historic))
        .route("/total-transactions", get(total_transactions))
        .route(
            "/total-transactions/historic",
            get(total_transactions_historic),
        )
        .route("/tps", get(tps))
        .route("/tps/historic", get(tps_historic))
        .route("/tps/peak", get(tps_peak))
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
        Some(sample) => {
            match decode_payload::<TotalTransactionsPayload>(&sample, "total-transactions") {
                Some(payload) => (
                    Some(payload.total_transactions),
                    Some(sample.recorded_at_ms),
                ),
                None => (None, None),
            }
        }
        None => (None, None),
    };

    Json(TotalTransactionsResponse {
        total_transactions,
        as_of_ms,
    })
}

#[utoipa::path(
    get,
    path = "/total-transactions/historic",
    params(
        ("from_ms" = Option<i64>, Query, description = "Start of the range (milliseconds since epoch). Requires `to_ms`."),
        ("to_ms" = Option<i64>, Query, description = "End of the range (milliseconds since epoch). Requires `from_ms`.")
    ),
    responses(
        (status = 200, description = "Historical transaction totals", body = TotalTransactionsSeriesSnapshot)
    ),
    tag = "metrics"
)]
async fn total_transactions_historic(
    State(state): State<AppState>,
    Query(params): Query<HistoricRangeQuery>,
) -> Json<TotalTransactionsSeriesSnapshot> {
    let range = resolve_range(params.from_ms, params.to_ms);
    let mut series = load_series(&state.store, "total-transactions", range)
        .await
        .map(map_total_transactions_series)
        .unwrap_or_else(|| TotalTransactionsSeriesSnapshot {
            name: "total-transactions".to_string(),
            interval_secs: 0,
            latest: None,
            samples: Vec::new(),
        });

    series.samples = filter_samples_by_range(series.samples, range, |sample| sample.recorded_at_ms);
    series.latest = series.samples.last().cloned();

    Json(series)
}

#[utoipa::path(
    get,
    path = "/tps",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Window size in seconds used to compute average slot TPS. Defaults to latest slot TPS.")
    ),
    responses(
        (status = 200, description = "Slot-based TPS derived from /ledger/tps", body = TpsResponse)
    ),
    tag = "metrics"
)]
async fn tps(
    State(state): State<AppState>,
    Query(params): Query<WindowQuery>,
) -> Json<TpsResponse> {
    let window_ms = window_ms(params.window_seconds);
    let latest_total = latest_total_transactions(&state).await;

    let (tps, delta_transactions, delta_ms) = match window_ms {
        Some(window_ms) if window_ms > 0 => {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let from_ms = now_ms.saturating_sub(window_ms);
            let samples = fetch_slot_tps_samples(&state, Some(from_ms), Some(now_ms)).await;
            aggregate_slot_tps(&samples)
        }
        _ => match fetch_latest_slot_tps(&state).await {
            Some(sample) => (
                Some(sample.tps),
                Some(sample.tx_count),
                i64::try_from(sample.block_time_ms).ok(),
            ),
            None => (None, None, None),
        },
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
    path = "/tps/historic",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Window size in seconds used to compute rolling average slot TPS per sample. Defaults to per-slot TPS."),
        ("from_ms" = Option<i64>, Query, description = "Start of the range (milliseconds since epoch). Requires `to_ms`."),
        ("to_ms" = Option<i64>, Query, description = "End of the range (milliseconds since epoch). Requires `from_ms`.")
    ),
    responses(
        (status = 200, description = "Historical slot-based TPS samples", body = TpsSeriesSnapshot)
    ),
    tag = "metrics"
)]
async fn tps_historic(
    State(state): State<AppState>,
    Query(params): Query<HistoricWindowQuery>,
) -> Json<TpsSeriesSnapshot> {
    let range = resolve_range(params.from_ms, params.to_ms);
    let window_ms = window_ms(params.window_seconds);
    let now_ms = chrono::Utc::now().timestamp_millis();

    let (query_from_ms, query_to_ms) = match range {
        Some((from_ms, to_ms)) => {
            let padded_from_ms = match window_ms {
                Some(window_ms) if window_ms > 0 => from_ms.saturating_sub(window_ms),
                _ => from_ms,
            };
            (Some(padded_from_ms), Some(to_ms))
        }
        None => {
            let lookback_ms = window_ms.unwrap_or(DEFAULT_TPS_HISTORIC_LOOKBACK_SECONDS * 1000);
            let from_ms = now_ms.saturating_sub(lookback_ms);
            (Some(from_ms), Some(now_ms))
        }
    };

    let slot_samples = fetch_slot_tps_samples(&state, query_from_ms, query_to_ms).await;
    let mut samples = derive_historic_tps_from_slot_samples(&slot_samples, window_ms);
    samples = filter_samples_by_range(samples, range, |sample| sample.recorded_at_ms);
    let latest = samples.last().cloned();

    Json(TpsSeriesSnapshot {
        name: "tps".to_string(),
        interval_secs: 0,
        latest,
        samples,
    })
}

/// Cache is valid if computed within this threshold (30 seconds).
const TPS_PEAK_CACHE_THRESHOLD_MS: i64 = 30 * 1000;

#[utoipa::path(
    get,
    path = "/tps/peak",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Window size in seconds to search for peak TPS. Defaults to 300 (5 minutes)."),
        ("no_cache" = Option<bool>, Query, description = "Set to true to bypass cache and force fresh calculation.")
    ),
    responses(
        (status = 200, description = "Peak TPS in the specified window", body = TpsPeakResponse)
    ),
    tag = "metrics"
)]
async fn tps_peak(
    State(state): State<AppState>,
    Query(params): Query<TpsPeakQuery>,
) -> Json<TpsPeakResponse> {
    let window_secs = params.window_seconds.unwrap_or(300); // 5 minutes default
    let window_ms = (window_secs as i64) * 1000;
    let no_cache = params.no_cache.unwrap_or(false);

    let now = chrono::Utc::now();
    let now_ms = now.timestamp_millis();
    let window_start_ms = now_ms - window_ms;

    // Check cache first (unless no_cache is set)
    if !no_cache {
        let cache = state.tps_peak_cache.inner.read().await;
        if let Some(ref entry) = *cache {
            // Cache is valid if:
            // 1. It was computed recently (within threshold)
            // 2. It was computed for the same requested window size
            let cache_age_ms = now_ms - entry.computed_at_ms;
            if cache_age_ms < TPS_PEAK_CACHE_THRESHOLD_MS && entry.window_ms == window_ms {
                // Apply multiplier to cached value
                return Json(TpsPeakResponse {
                    peak_tps: Some(apply_peak_tps_multiplier(
                        entry.peak_tps,
                        state.peak_tps_multiplier,
                    )),
                    peak_at_ms: Some(entry.peak_at_ms),
                    window_ms,
                    from_cache: true,
                });
            }
        }
    }

    // Cache miss or stale - query ledger /tps endpoints and find the peak in the requested window.
    let samples = fetch_slot_tps_samples(&state, Some(window_start_ms), Some(now_ms)).await;
    let (peak_tps, peak_at_ms) = peak_tps_from_samples(&samples);

    // Update cache (store raw value before multiplier)
    if let (Some(tps), Some(at_ms)) = (peak_tps, peak_at_ms) {
        let mut cache = state.tps_peak_cache.inner.write().await;
        *cache = Some(TpsPeakCacheEntry {
            peak_tps: tps,
            peak_at_ms: at_ms,
            window_ms,
            computed_at_ms: now_ms,
        });
    }

    // Apply multiplier to output
    let peak_tps_output =
        peak_tps.map(|tps| apply_peak_tps_multiplier(tps, state.peak_tps_multiplier));

    Json(TpsPeakResponse {
        peak_tps: peak_tps_output,
        peak_at_ms,
        window_ms,
        from_cache: false,
    })
}

// =============================================================================
// EMA Metrics Endpoints (MockMCP-compatible /metrics/{s2|s5|m1|m5|m15})
// =============================================================================

#[utoipa::path(
    get,
    path = "/s2",
    responses(
        (status = 200, description = "2-second EMA metrics", body = EmaMetricsResponse)
    ),
    tag = "ema-metrics"
)]
async fn metrics_s2(State(state): State<AppState>) -> Json<EmaMetricsResponse> {
    Json(compute_ema_metrics(&state, EmaWindow::S2).await)
}

#[utoipa::path(
    get,
    path = "/s5",
    responses(
        (status = 200, description = "5-second EMA metrics", body = EmaMetricsResponse)
    ),
    tag = "ema-metrics"
)]
async fn metrics_s5(State(state): State<AppState>) -> Json<EmaMetricsResponse> {
    Json(compute_ema_metrics(&state, EmaWindow::S5).await)
}

#[utoipa::path(
    get,
    path = "/m1",
    responses(
        (status = 200, description = "1-minute EMA metrics", body = EmaMetricsResponse)
    ),
    tag = "ema-metrics"
)]
async fn metrics_m1(State(state): State<AppState>) -> Json<EmaMetricsResponse> {
    Json(compute_ema_metrics(&state, EmaWindow::M1).await)
}

#[utoipa::path(
    get,
    path = "/m5",
    responses(
        (status = 200, description = "5-minute EMA metrics", body = EmaMetricsResponse)
    ),
    tag = "ema-metrics"
)]
async fn metrics_m5(State(state): State<AppState>) -> Json<EmaMetricsResponse> {
    Json(compute_ema_metrics(&state, EmaWindow::M5).await)
}

#[utoipa::path(
    get,
    path = "/m15",
    responses(
        (status = 200, description = "15-minute EMA metrics", body = EmaMetricsResponse)
    ),
    tag = "ema-metrics"
)]
async fn metrics_m15(State(state): State<AppState>) -> Json<EmaMetricsResponse> {
    Json(compute_ema_metrics(&state, EmaWindow::M15).await)
}

/// Computes EMA metrics for a given window.
///
/// This aggregates data from multiple metric series to produce the MockMCP-compatible
/// response format.
async fn compute_ema_metrics(state: &AppState, window: EmaWindow) -> EmaMetricsResponse {
    // Get accounts data (latest snapshot)
    let accounts_data = state
        .store
        .snapshot("accounts")
        .await
        .and_then(|s| s.latest)
        .and_then(|sample| decode_payload::<AccountsPayload>(&sample, "accounts"));

    // Get total transactions data for cumulative totals.
    let total_tx_series = state
        .store
        .snapshot("total-transactions")
        .await
        .map(map_total_transactions_series);

    // Get token value spent data for TokensPerSecond calculation
    let token_value_series = state.store.snapshot("token-value-spent").await;

    // Get total tokens economy for TotalTokensInWallets
    let total_tokens_data = state
        .store
        .snapshot("total-tokens-economy")
        .await
        .and_then(|s| s.latest)
        .and_then(|sample| {
            decode_payload::<TotalTokensEconomyPayload>(&sample, "total-tokens-economy")
        });

    let now_ms = chrono::Utc::now().timestamp_millis();
    let window_ms = i64::try_from(window.seconds())
        .ok()
        .and_then(|secs| secs.checked_mul(1000))
        .unwrap_or(0);
    let window_start_ms = now_ms.saturating_sub(window_ms);

    // Query slot TPS samples for the current EMA window.
    let slot_tps_samples = fetch_slot_tps_samples(state, Some(window_start_ms), Some(now_ms)).await;

    // Calculate TPS using EMA over per-slot TPS values.
    let tps = compute_ema_from_samples(
        slot_tps_samples
            .iter()
            .map(|sample| (sample.timestamp_ms, sample.tps)),
        window,
    );

    // Calculate TokensPerSecond using EMA
    let tokens_per_second = token_value_series.as_ref().and_then(|series| {
        let samples: Vec<(i64, u128)> = series
            .samples
            .iter()
            .filter_map(|s| {
                let payload: TokenValueSpentPayload = decode_payload(s, "token-value-spent")?;
                let amount = payload.total_amount.parse::<u128>().ok()?;
                Some((s.recorded_at_ms, amount))
            })
            .collect();
        compute_tokens_per_second_ema(samples, window)
    });

    // Get cumulative totals
    let total_transactions = total_tx_series
        .as_ref()
        .and_then(|s| s.latest.as_ref())
        .map(|s| s.payload.total_transactions)
        .unwrap_or(0);

    let total_tokens_in_wallets = total_tokens_data
        .as_ref()
        .and_then(|p| p.total_amount.parse::<u64>().ok())
        .unwrap_or(0);

    let accounts = accounts_data
        .as_ref()
        .map(|p| p.total_accounts)
        .unwrap_or(0);
    let sending_accounts = accounts_data
        .as_ref()
        .map(|p| p.sending_accounts)
        .unwrap_or(0);
    let total_disclosure_events = accounts_data
        .as_ref()
        .map(|p| p.total_disclosure_events)
        .unwrap_or(0);

    // Compute peak TPS for this EMA window from slot samples.
    let (peak_tps, peak_tps_at_ms) = match peak_tps_from_samples(&slot_tps_samples) {
        (Some(tps), peak_at_ms) => (tps, peak_at_ms),
        (None, _) => (0.0, None),
    };

    // Apply peak TPS multiplier
    let peak_tps = apply_peak_tps_multiplier(peak_tps, state.peak_tps_multiplier);

    // Round TPS values to 2 decimal places to avoid showing tiny numbers
    let tps = round_to_precision(tps.unwrap_or(0.0), 2);
    let peak_tps = round_to_precision(peak_tps, 2);
    let tokens_per_second = round_to_precision(tokens_per_second.unwrap_or(0.0), 2);

    EmaMetricsResponse {
        accounts,
        sending_accounts,
        tps,
        peak_tps,
        peak_tps_at_ms,
        tokens_per_second,
        total_disclosure_events,
        total_tokens_in_wallets,
        total_transactions,
    }
}

/// Rounds a f64 value to a specified number of decimal places.
fn round_to_precision(value: f64, decimals: u32) -> f64 {
    let multiplier = 10_f64.powi(decimals as i32);
    (value * multiplier).round() / multiplier
}

/// Applies the peak TPS multiplier without randomization.
fn apply_peak_tps_multiplier(value: f64, multiplier: f64) -> f64 {
    value * multiplier
}

#[derive(Debug, Deserialize)]
struct LedgerSlotTpsPayload {
    slot_number: u64,
    tx_count: u64,
    block_time_ms: u64,
    tps: f64,
    timestamp: serde_json::Value,
}

#[derive(Debug, Clone)]
struct LedgerSlotTpsSample {
    slot_number: u64,
    tx_count: u64,
    block_time_ms: u64,
    tps: f64,
    timestamp_ms: i64,
}

fn ledger_tps_url(base_url: &str, slot_path: &str) -> String {
    format!(
        "{}/ledger/tps/{}",
        base_url.trim_end_matches('/'),
        slot_path.trim_start_matches('/')
    )
}

fn parse_ledger_timestamp_ms(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|ts| ts.round() as i64)),
        serde_json::Value::String(value) => value
            .parse::<i64>()
            .ok()
            .or_else(|| value.parse::<f64>().ok().map(|ts| ts.round() as i64)),
        _ => None,
    }
}

async fn fetch_slot_tps_by_path(state: &AppState, slot_path: &str) -> Option<LedgerSlotTpsSample> {
    let url = ledger_tps_url(&state.ledger_api_base_url, slot_path);
    let response = match state.ledger_http_client.get(&url).send().await {
        Ok(response) => response,
        Err(error) => {
            warn!(url, error = %error, "Failed to request ledger slot TPS");
            return None;
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        if status != reqwest::StatusCode::NOT_FOUND && status != reqwest::StatusCode::BAD_REQUEST {
            warn!(url, status = %status, "Ledger slot TPS request failed");
        }
        return None;
    }

    let payload = match response.json::<LedgerSlotTpsPayload>().await {
        Ok(payload) => payload,
        Err(error) => {
            warn!(url, error = %error, "Failed to parse ledger slot TPS response");
            return None;
        }
    };

    let timestamp_ms = match parse_ledger_timestamp_ms(&payload.timestamp) {
        Some(ts) => ts,
        None => {
            warn!(url, "Ledger slot TPS response had invalid timestamp");
            return None;
        }
    };

    Some(LedgerSlotTpsSample {
        slot_number: payload.slot_number,
        tx_count: payload.tx_count,
        block_time_ms: payload.block_time_ms,
        tps: payload.tps,
        timestamp_ms,
    })
}

async fn fetch_latest_slot_tps(state: &AppState) -> Option<LedgerSlotTpsSample> {
    fetch_slot_tps_by_path(state, "latest").await
}

async fn fetch_slot_tps_by_number(
    state: &AppState,
    slot_number: u64,
) -> Option<LedgerSlotTpsSample> {
    fetch_slot_tps_by_path(state, &slot_number.to_string()).await
}

async fn fetch_slot_tps_samples(
    state: &AppState,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
) -> Vec<LedgerSlotTpsSample> {
    let mut out = Vec::new();

    let mut current = match fetch_latest_slot_tps(state).await {
        Some(sample) => sample,
        None => return out,
    };

    for _ in 0..MAX_LEDGER_TPS_SLOTS {
        if let Some(to_ms) = to_ms {
            if current.timestamp_ms > to_ms {
                if current.slot_number <= 1 {
                    break;
                }
                let next_slot = current.slot_number - 1;
                current = match fetch_slot_tps_by_number(state, next_slot).await {
                    Some(sample) => sample,
                    None => break,
                };
                continue;
            }
        }

        if let Some(from_ms) = from_ms {
            if current.timestamp_ms < from_ms {
                break;
            }
        }

        out.push(current.clone());

        if current.slot_number <= 1 {
            break;
        }
        let next_slot = current.slot_number - 1;
        current = match fetch_slot_tps_by_number(state, next_slot).await {
            Some(sample) => sample,
            None => break,
        };
    }

    out.reverse();
    out
}

fn aggregate_slot_tps(samples: &[LedgerSlotTpsSample]) -> (Option<f64>, Option<u64>, Option<i64>) {
    if samples.is_empty() {
        return (None, None, None);
    }

    let delta_transactions_u128 = samples
        .iter()
        .fold(0u128, |acc, sample| acc + u128::from(sample.tx_count));
    let delta_ms_u128 = samples
        .iter()
        .fold(0u128, |acc, sample| acc + u128::from(sample.block_time_ms));
    if delta_ms_u128 == 0 {
        return (None, u64::try_from(delta_transactions_u128).ok(), Some(0));
    }

    let tps = (delta_transactions_u128 as f64) / (delta_ms_u128 as f64 / 1000.0);

    (
        Some(tps),
        u64::try_from(delta_transactions_u128).ok(),
        i64::try_from(delta_ms_u128).ok(),
    )
}

fn derive_historic_tps_from_slot_samples(
    samples: &[LedgerSlotTpsSample],
    window_ms: Option<i64>,
) -> Vec<TpsSample> {
    if samples.is_empty() {
        return Vec::new();
    }

    match window_ms {
        Some(window_ms) if window_ms > 0 => {
            let mut derived = Vec::with_capacity(samples.len());
            let mut start_idx = 0usize;
            let mut sum_tx = 0u128;
            let mut sum_ms = 0u128;

            for (idx, sample) in samples.iter().enumerate() {
                sum_tx += u128::from(sample.tx_count);
                sum_ms += u128::from(sample.block_time_ms);

                let cutoff = sample.timestamp_ms.saturating_sub(window_ms);
                while start_idx <= idx && samples[start_idx].timestamp_ms < cutoff {
                    sum_tx = sum_tx.saturating_sub(u128::from(samples[start_idx].tx_count));
                    sum_ms = sum_ms.saturating_sub(u128::from(samples[start_idx].block_time_ms));
                    start_idx += 1;
                }

                if sum_ms == 0 {
                    continue;
                }

                let tps = (sum_tx as f64) / (sum_ms as f64 / 1000.0);
                derived.push(TpsSample {
                    recorded_at_ms: sample.timestamp_ms,
                    tps: Some(tps),
                    delta_transactions: u64::try_from(sum_tx).ok(),
                    delta_ms: i64::try_from(sum_ms).ok(),
                    latest_total: None,
                });
            }

            derived
        }
        _ => samples
            .iter()
            .map(|sample| TpsSample {
                recorded_at_ms: sample.timestamp_ms,
                tps: Some(sample.tps),
                delta_transactions: Some(sample.tx_count),
                delta_ms: i64::try_from(sample.block_time_ms).ok(),
                latest_total: None,
            })
            .collect(),
    }
}

fn peak_tps_from_samples(samples: &[LedgerSlotTpsSample]) -> (Option<f64>, Option<i64>) {
    let peak = samples.iter().max_by(|a, b| {
        a.tps
            .partial_cmp(&b.tps)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    match peak {
        Some(sample) => (Some(sample.tps), Some(sample.timestamp_ms)),
        None => (None, None),
    }
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
        retention_seconds: state.retention_secs,
    })
}

#[utoipa::path(
    get,
    path = "/failed-transactions-rate/historic",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Window size in seconds used to compute the failure rate per sample. Defaults to the last two samples."),
        ("from_ms" = Option<i64>, Query, description = "Start of the range (milliseconds since epoch). Requires `to_ms`."),
        ("to_ms" = Option<i64>, Query, description = "End of the range (milliseconds since epoch). Requires `from_ms`.")
    ),
    responses(
        (status = 200, description = "Historical failure rate samples", body = FailedTransactionsRateSeriesSnapshot)
    ),
    tag = "metrics"
)]
async fn failed_transactions_rate_historic(
    State(state): State<AppState>,
    Query(params): Query<HistoricWindowQuery>,
) -> Json<FailedTransactionsRateSeriesSnapshot> {
    let range = resolve_range(params.from_ms, params.to_ms);
    let window_ms = window_ms(params.window_seconds);
    let query_range = extend_range(range, window_ms);

    let series = load_series(&state.store, "failed-transactions-rate", query_range)
        .await
        .map(map_failed_transactions_series)
        .unwrap_or_else(|| FailedTransactionsSeriesSnapshot {
            name: "failed-transactions-rate".to_string(),
            interval_secs: 0,
            latest: None,
            samples: Vec::new(),
        });

    let mut samples = derive_series_with_window(
        &series.samples,
        window_ms,
        |sample| sample.recorded_at_ms,
        |start, latest, delta_ms| {
            let delta_total = latest
                .payload
                .total_transactions
                .saturating_sub(start.payload.total_transactions);
            let delta_failed = latest
                .payload
                .failed_transactions
                .saturating_sub(start.payload.failed_transactions);

            let rate_percent = if delta_total == 0 {
                None
            } else {
                Some((delta_failed as f64 / delta_total as f64) * 100.0)
            };

            Some(FailedTransactionsRateSample {
                recorded_at_ms: latest.recorded_at_ms,
                rate_percent,
                failed_transactions: Some(delta_failed),
                total_transactions: Some(delta_total),
                delta_ms: Some(delta_ms),
            })
        },
    );

    samples = filter_samples_by_range(samples, range, |sample| sample.recorded_at_ms);
    let latest = samples.last().cloned();

    Json(FailedTransactionsRateSeriesSnapshot {
        name: "failed-transactions-rate".to_string(),
        interval_secs: series.interval_secs,
        latest,
        samples,
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
    let window_ms = match params.window_seconds {
        Some(window_seconds) => window_ms(Some(window_seconds)),
        None => window_ms(Some(86_400)),
    };

    let backend = {
        use sea_orm::ConnectionTrait;
        state.indexer_db.get_database_backend()
    };
    if should_query_average_from_indexer(backend, window_ms) {
        use sea_orm::{FromQueryResult, Statement};

        #[derive(Debug, FromQueryResult)]
        struct AverageRow {
            average_amount: Option<f64>,
            delta_amount: String,
            delta_transactions: i64,
        }

        let now = chrono::Utc::now();
        let now_ms = now.timestamp_millis();
        let start_ms = window_ms
            .and_then(|window_ms| now_ms.checked_sub(window_ms))
            .unwrap_or(now_ms);
        let delta_ms = now_ms.saturating_sub(start_ms);
        let start = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(start_ms).unwrap_or(now);

        let stmt = Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            POSTGRES_AVERAGE_TRANSACTION_SIZE_SQL,
            [start.into(), now.into()],
        );

        match AverageRow::find_by_statement(stmt)
            .one(&state.indexer_db)
            .await
        {
            Ok(Some(row)) => {
                let delta_transactions = u64::try_from(row.delta_transactions).ok();
                return Json(AverageTransactionSizeResponse {
                    average_amount: row.average_amount,
                    delta_amount: Some(row.delta_amount),
                    delta_transactions,
                    delta_ms: Some(delta_ms),
                    retention_seconds: state.retention_secs,
                });
            }
            Ok(None) => {
                return Json(AverageTransactionSizeResponse {
                    average_amount: None,
                    delta_amount: Some("0".to_string()),
                    delta_transactions: Some(0),
                    delta_ms: Some(delta_ms),
                    retention_seconds: state.retention_secs,
                });
            }
            Err(error) => {
                warn!(error = %error, "Failed to query average transaction size from indexer DB");
                return Json(AverageTransactionSizeResponse {
                    average_amount: None,
                    delta_amount: Some("0".to_string()),
                    delta_transactions: Some(0),
                    delta_ms: Some(delta_ms),
                    retention_seconds: state.retention_secs,
                });
            }
        }
    }

    let series_snapshot = state
        .store
        .snapshot("token-value-spent")
        .await
        .map(map_average_transaction_size_series);
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
        retention_seconds: state.retention_secs,
    })
}

#[utoipa::path(
    get,
    path = "/average-transaction-size/historic",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Window size in seconds used to compute the average amount per sample. Defaults to 24 hours."),
        ("from_ms" = Option<i64>, Query, description = "Start of the range (milliseconds since epoch). Requires `to_ms`."),
        ("to_ms" = Option<i64>, Query, description = "End of the range (milliseconds since epoch). Requires `from_ms`.")
    ),
    responses(
        (status = 200, description = "Historical average transaction size samples", body = AverageTransactionSizeHistoricSeriesSnapshot)
    ),
    tag = "metrics"
)]
async fn average_transaction_size_historic(
    State(state): State<AppState>,
    Query(params): Query<HistoricWindowQuery>,
) -> Json<AverageTransactionSizeHistoricSeriesSnapshot> {
    let range = resolve_range(params.from_ms, params.to_ms);
    let window_ms = match params.window_seconds {
        Some(window_seconds) => window_ms(Some(window_seconds)),
        None => window_ms(Some(86_400)),
    };
    let query_range = extend_range(range, window_ms);

    let series = load_series(&state.store, "token-value-spent", query_range)
        .await
        .map(map_average_transaction_size_series)
        .unwrap_or_else(|| AverageTransactionSizeSeriesSnapshot {
            name: "average-transaction-size".to_string(),
            interval_secs: 0,
            latest: None,
            samples: Vec::new(),
        });

    let mut samples = derive_series_with_window(
        &series.samples,
        window_ms,
        |sample| sample.recorded_at_ms,
        |start, latest, delta_ms| {
            let prev_total = parse_amount(&start.payload.total_amount)?;
            let latest_total = parse_amount(&latest.payload.total_amount)?;
            let delta_amount = latest_total.saturating_sub(prev_total);
            let delta_transactions = latest
                .payload
                .total_transactions
                .saturating_sub(start.payload.total_transactions);
            let average_amount = if delta_transactions == 0 {
                None
            } else {
                Some(delta_amount as f64 / delta_transactions as f64)
            };

            Some(AverageTransactionSizeHistoricSample {
                recorded_at_ms: latest.recorded_at_ms,
                average_amount,
                delta_amount: Some(delta_amount.to_string()),
                delta_transactions: Some(delta_transactions),
                delta_ms: Some(delta_ms),
            })
        },
    );

    samples = filter_samples_by_range(samples, range, |sample| sample.recorded_at_ms);
    let latest = samples.last().cloned();

    Json(AverageTransactionSizeHistoricSeriesSnapshot {
        name: "average-transaction-size".to_string(),
        interval_secs: series.interval_secs,
        latest,
        samples,
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

    let now = chrono::Utc::now();
    let now_ms = now.timestamp_millis();
    let start_ms = window_ms
        .and_then(|window_ms| now_ms.checked_sub(window_ms))
        .unwrap_or(now_ms);

    let backend = {
        use sea_orm::ConnectionTrait;
        state.indexer_db.get_database_backend()
    };
    if should_query_median_from_indexer(backend) {
        use sea_orm::{FromQueryResult, Statement};

        #[derive(Debug, FromQueryResult)]
        struct MedianRow {
            median_amount: Option<f64>,
        }

        let start = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(start_ms).unwrap_or(now);
        let stmt = Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            POSTGRES_MEDIAN_TRANSACTION_SIZE_SQL,
            [start.into(), now.into()],
        );

        match MedianRow::find_by_statement(stmt)
            .one(&state.indexer_db)
            .await
        {
            Ok(row) => {
                return Json(MedianTransactionSizeResponse {
                    median_amount: row.and_then(|row| row.median_amount),
                });
            }
            Err(error) => {
                warn!(error = %error, "Failed to query median transaction size from indexer DB");
                return Json(MedianTransactionSizeResponse {
                    median_amount: None,
                });
            }
        }
    }

    let mut values = state
        .store
        .values_in_range("transaction-size", start_ms, now_ms)
        .await
        .unwrap_or_default();

    values.retain(|value| value.is_finite());
    if values.is_empty() {
        return Json(MedianTransactionSizeResponse {
            median_amount: None,
        });
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
    path = "/median-transaction-size/historic",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Bucket size in seconds used to compute median values. Defaults to 24 hours."),
        ("from_ms" = Option<i64>, Query, description = "Start of the range (milliseconds since epoch). Requires `to_ms`."),
        ("to_ms" = Option<i64>, Query, description = "End of the range (milliseconds since epoch). Requires `from_ms`.")
    ),
    responses(
        (status = 200, description = "Historical median transaction sizes", body = MedianTransactionSizeSeriesSnapshot)
    ),
    tag = "metrics"
)]
async fn median_transaction_size_historic(
    State(state): State<AppState>,
    Query(params): Query<HistoricWindowQuery>,
) -> Json<MedianTransactionSizeSeriesSnapshot> {
    let range = resolve_range(params.from_ms, params.to_ms);
    let (bucket_ms, bucket_seconds) = median_bucket_ms(params.window_seconds);

    let series = load_series(&state.store, "transaction-size", range)
        .await
        .unwrap_or_else(|| MetricSeriesSnapshot {
            name: "transaction-size".to_string(),
            interval_secs: 0,
            latest: None,
            samples: Vec::new(),
        });

    let samples = filter_samples_by_range(series.samples, range, |sample| sample.recorded_at_ms);
    let mut buckets: BTreeMap<i64, Vec<f64>> = BTreeMap::new();

    for sample in samples {
        let payload: TransactionSizePayload = match decode_payload(&sample, "transaction-size") {
            Some(payload) => payload,
            None => continue,
        };
        let value = match parse_amount(&payload.amount) {
            Some(value) => value as f64,
            None => continue,
        };

        let bucket_start = sample.recorded_at_ms.div_euclid(bucket_ms) * bucket_ms;
        buckets.entry(bucket_start).or_default().push(value);
    }

    let mut median_samples = Vec::with_capacity(buckets.len());
    for (bucket_start, mut values) in buckets {
        values.retain(|value| value.is_finite());
        if values.is_empty() {
            continue;
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = values.len() / 2;
        let median = if values.len() % 2 == 0 {
            (values[mid - 1] + values[mid]) / 2.0
        } else {
            values[mid]
        };

        median_samples.push(MedianTransactionSizeSample {
            recorded_at_ms: bucket_start,
            median_amount: Some(median),
        });
    }

    let latest = median_samples.last().cloned();

    Json(MedianTransactionSizeSeriesSnapshot {
        name: "median-transaction-size".to_string(),
        bucket_seconds,
        latest,
        samples: median_samples,
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

    Json(TokenValueSpentResponse { value_spent })
}

#[utoipa::path(
    get,
    path = "/token-value-spent/historic",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Window size in seconds used to compute token value spent per sample. Defaults to 24 hours."),
        ("from_ms" = Option<i64>, Query, description = "Start of the range (milliseconds since epoch). Requires `to_ms`."),
        ("to_ms" = Option<i64>, Query, description = "End of the range (milliseconds since epoch). Requires `from_ms`.")
    ),
    responses(
        (status = 200, description = "Historical token value spent samples", body = TokenValueSpentHistoricSeriesSnapshot)
    ),
    tag = "metrics"
)]
async fn token_value_spent_historic(
    State(state): State<AppState>,
    Query(params): Query<HistoricWindowQuery>,
) -> Json<TokenValueSpentHistoricSeriesSnapshot> {
    let range = resolve_range(params.from_ms, params.to_ms);
    let window_ms = match params.window_seconds {
        Some(window_seconds) => window_ms(Some(window_seconds)),
        None => window_ms(Some(86_400)),
    };
    let query_range = extend_range(range, window_ms);

    let series = load_series(&state.store, "token-value-spent", query_range)
        .await
        .unwrap_or_else(|| MetricSeriesSnapshot {
            name: "token-value-spent".to_string(),
            interval_secs: 0,
            latest: None,
            samples: Vec::new(),
        });

    let mut samples = derive_series_with_window(
        &series.samples,
        window_ms,
        |sample| sample.recorded_at_ms,
        |start, latest, delta_ms| {
            let value_spent = token_value_spent_from_samples(start, latest)?;
            Some(TokenValueSpentHistoricSample {
                recorded_at_ms: latest.recorded_at_ms,
                value_spent: Some(value_spent),
                delta_ms: Some(delta_ms),
            })
        },
    );

    samples = filter_samples_by_range(samples, range, |sample| sample.recorded_at_ms);
    let latest = samples.last().cloned();

    Json(TokenValueSpentHistoricSeriesSnapshot {
        name: "token-value-spent".to_string(),
        interval_secs: series.interval_secs,
        latest,
        samples,
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
                warn!(
                    from_ms,
                    to_ms, "Negative token velocity range not supported"
                );
                None
            } else {
                Some((from_ms, to_ms))
            }
        }
        _ => None,
    };

    let value_spent =
        state
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

#[utoipa::path(
    get,
    path = "/token-velocity/historic",
    params(
        ("window_seconds" = Option<u64>, Query, description = "Window size in seconds used to compute token velocity per sample. Defaults to 24 hours."),
        ("from_ms" = Option<i64>, Query, description = "Start of the range (milliseconds since epoch). Requires `to_ms`."),
        ("to_ms" = Option<i64>, Query, description = "End of the range (milliseconds since epoch). Requires `from_ms`.")
    ),
    responses(
        (status = 200, description = "Historical token velocity samples", body = TokenVelocityHistoricSeriesSnapshot)
    ),
    tag = "metrics"
)]
async fn token_velocity_historic(
    State(state): State<AppState>,
    Query(params): Query<HistoricWindowQuery>,
) -> Json<TokenVelocityHistoricSeriesSnapshot> {
    let range = resolve_range(params.from_ms, params.to_ms);
    let window_ms = match params.window_seconds {
        Some(window_seconds) => window_ms(Some(window_seconds)),
        None => window_ms(Some(86_400)),
    };
    let query_range = extend_range(range, window_ms);

    let value_series = load_series(&state.store, "token-value-spent", query_range)
        .await
        .unwrap_or_else(|| MetricSeriesSnapshot {
            name: "token-value-spent".to_string(),
            interval_secs: 0,
            latest: None,
            samples: Vec::new(),
        });
    let total_series = load_series(&state.store, "total-tokens-economy", query_range)
        .await
        .unwrap_or_else(|| MetricSeriesSnapshot {
            name: "total-tokens-economy".to_string(),
            interval_secs: 0,
            latest: None,
            samples: Vec::new(),
        });

    let mut samples =
        derive_token_velocity_series(&value_series.samples, &total_series.samples, window_ms);

    samples = filter_samples_by_range(samples, range, |sample| sample.recorded_at_ms);
    let latest = samples.last().cloned();

    Json(TokenVelocityHistoricSeriesSnapshot {
        name: "token-velocity".to_string(),
        interval_secs: value_series.interval_secs,
        latest,
        samples,
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

fn should_query_average_from_indexer(
    backend: sea_orm::DatabaseBackend,
    window_ms: Option<i64>,
) -> bool {
    backend == sea_orm::DatabaseBackend::Postgres && window_ms.is_some()
}

fn should_query_median_from_indexer(backend: sea_orm::DatabaseBackend) -> bool {
    backend == sea_orm::DatabaseBackend::Postgres
}

fn resolve_range(from_ms: Option<i64>, to_ms: Option<i64>) -> Option<(i64, i64)> {
    match (from_ms, to_ms) {
        (Some(from_ms), Some(to_ms)) => {
            if from_ms >= to_ms {
                warn!(from_ms, to_ms, "Invalid historic range");
                None
            } else if from_ms < 0 || to_ms < 0 {
                warn!(from_ms, to_ms, "Negative historic range not supported");
                None
            } else {
                Some((from_ms, to_ms))
            }
        }
        _ => None,
    }
}

fn extend_range(range: Option<(i64, i64)>, window_ms: Option<i64>) -> Option<(i64, i64)> {
    match (range, window_ms) {
        (Some((start_ms, end_ms)), Some(window_ms)) => {
            Some((start_ms.saturating_sub(window_ms), end_ms))
        }
        (Some(range), None) => Some(range),
        _ => None,
    }
}

async fn load_series(
    store: &MetricsStore,
    name: &'static str,
    range: Option<(i64, i64)>,
) -> Option<MetricSeriesSnapshot> {
    match range {
        Some((start_ms, end_ms)) => store.snapshot_range(name, start_ms, end_ms).await,
        None => store.snapshot(name).await,
    }
}

async fn latest_total_transactions(state: &AppState) -> Option<u64> {
    state
        .store
        .snapshot("total-transactions")
        .await
        .map(map_total_transactions_series)
        .and_then(|series| series.latest.or_else(|| series.samples.last().cloned()))
        .map(|sample| sample.payload.total_transactions)
}

fn filter_samples_by_range<T, F>(samples: Vec<T>, range: Option<(i64, i64)>, timestamp: F) -> Vec<T>
where
    F: Fn(&T) -> i64,
{
    let (from_ms, to_ms) = match range {
        Some(range) => range,
        None => return samples,
    };

    samples
        .into_iter()
        .filter(|sample| {
            let ts = timestamp(sample);
            ts >= from_ms && ts <= to_ms
        })
        .collect()
}

fn derive_series_with_window<T, U, F, TS>(
    samples: &[T],
    window_ms: Option<i64>,
    timestamp: TS,
    mut derive: F,
) -> Vec<U>
where
    F: FnMut(&T, &T, i64) -> Option<U>,
    TS: Fn(&T) -> i64,
{
    if samples.len() < 2 {
        return Vec::new();
    }

    let mut derived = Vec::new();

    match window_ms {
        Some(window_ms) if window_ms > 0 => {
            let mut start_idx = 0usize;
            for latest_idx in 1..samples.len() {
                let latest_ts = timestamp(&samples[latest_idx]);
                let target = latest_ts - window_ms;
                while start_idx + 1 < latest_idx && timestamp(&samples[start_idx + 1]) <= target {
                    start_idx += 1;
                }
                if start_idx >= latest_idx {
                    start_idx = latest_idx - 1;
                }
                let start_ts = timestamp(&samples[start_idx]);
                let delta_ms = latest_ts - start_ts;
                if delta_ms <= 0 {
                    continue;
                }
                if let Some(sample) = derive(&samples[start_idx], &samples[latest_idx], delta_ms) {
                    derived.push(sample);
                }
            }
        }
        _ => {
            for latest_idx in 1..samples.len() {
                let start = &samples[latest_idx - 1];
                let latest = &samples[latest_idx];
                let delta_ms = timestamp(latest) - timestamp(start);
                if delta_ms <= 0 {
                    continue;
                }
                if let Some(sample) = derive(start, latest, delta_ms) {
                    derived.push(sample);
                }
            }
        }
    }

    derived
}

fn median_bucket_ms(window_seconds: Option<u64>) -> (i64, u64) {
    let default_seconds = 86_400;
    let default_ms = window_ms(Some(default_seconds)).unwrap_or(86_400_000);

    match window_seconds {
        Some(seconds) => match window_ms(Some(seconds)) {
            Some(ms) => (ms, seconds),
            None => (default_ms, default_seconds),
        },
        None => (default_ms, default_seconds),
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

    let prev_payload: TokenValueSpentPayload = decode_payload(window.start, "token-value-spent")?;
    let latest_payload: TokenValueSpentPayload =
        decode_payload(window.latest, "token-value-spent")?;
    let prev_total = parse_amount(&prev_payload.total_amount)?;
    let latest_total = parse_amount(&latest_payload.total_amount)?;
    let delta_amount = latest_total.saturating_sub(prev_total);

    Some(delta_amount.to_string())
}

fn token_value_spent_from_samples(start: &MetricSample, latest: &MetricSample) -> Option<String> {
    let prev_payload: TokenValueSpentPayload = decode_payload(start, "token-value-spent")?;
    let latest_payload: TokenValueSpentPayload = decode_payload(latest, "token-value-spent")?;
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
    let window = select_range(&series.samples, from_ms, to_ms, |sample| {
        sample.recorded_at_ms
    })?;
    let prev_payload: TokenValueSpentPayload = decode_payload(window.start, "token-value-spent")?;
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
    let window = select_range(&series.samples, from_ms, to_ms, |sample| {
        sample.recorded_at_ms
    })?;
    total_tokens_average_from_samples(window.start, window.latest)
}

fn total_tokens_average_from_samples(
    start: &MetricSample,
    latest: &MetricSample,
) -> Option<String> {
    let start_payload: TotalTokensEconomyPayload = decode_payload(start, "total-tokens-economy")?;
    let latest_payload: TotalTokensEconomyPayload = decode_payload(latest, "total-tokens-economy")?;
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

fn total_tokens_average_for_window(
    samples: &[MetricSample],
    start_ms: i64,
    end_ms: i64,
    start_idx: &mut usize,
    end_idx: &mut usize,
) -> Option<String> {
    if samples.len() < 2 || start_ms >= end_ms {
        return None;
    }

    while *end_idx + 1 < samples.len() && samples[*end_idx + 1].recorded_at_ms <= end_ms {
        *end_idx += 1;
    }
    if samples[*end_idx].recorded_at_ms > end_ms {
        return None;
    }

    while *start_idx + 1 < samples.len() && samples[*start_idx + 1].recorded_at_ms <= start_ms {
        *start_idx += 1;
    }
    if *start_idx >= *end_idx {
        if *end_idx == 0 {
            return None;
        }
        *start_idx = *end_idx - 1;
    }

    total_tokens_average_from_samples(&samples[*start_idx], &samples[*end_idx])
}

fn derive_token_velocity_series(
    value_samples: &[MetricSample],
    total_samples: &[MetricSample],
    window_ms: Option<i64>,
) -> Vec<TokenVelocityHistoricSample> {
    if value_samples.len() < 2 {
        return Vec::new();
    }

    let mut derived = Vec::new();
    let mut value_start_idx = 0usize;
    let mut total_start_idx = 0usize;
    let mut total_end_idx = 0usize;

    for latest_idx in 1..value_samples.len() {
        let latest = &value_samples[latest_idx];
        let latest_ts = latest.recorded_at_ms;

        let start_idx = match window_ms {
            Some(window_ms) if window_ms > 0 => {
                let target = latest_ts - window_ms;
                while value_start_idx + 1 < latest_idx
                    && value_samples[value_start_idx + 1].recorded_at_ms <= target
                {
                    value_start_idx += 1;
                }
                if value_start_idx >= latest_idx {
                    value_start_idx = latest_idx - 1;
                }
                value_start_idx
            }
            _ => latest_idx - 1,
        };

        let start = &value_samples[start_idx];
        let delta_ms = latest_ts - start.recorded_at_ms;
        if delta_ms <= 0 {
            continue;
        }

        let value_spent = token_value_spent_from_samples(start, latest);
        let total_tokens = total_tokens_average_for_window(
            total_samples,
            start.recorded_at_ms,
            latest_ts,
            &mut total_start_idx,
            &mut total_end_idx,
        );

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

        derived.push(TokenVelocityHistoricSample {
            recorded_at_ms: latest_ts,
            token_velocity,
            value_spent,
            total_tokens,
            delta_ms: Some(delta_ms),
        });
    }

    derived
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
        latest,
        samples,
    }
}

fn map_failed_transactions_series(
    series: MetricSeriesSnapshot,
) -> FailedTransactionsSeriesSnapshot {
    let samples: Vec<FailedTransactionsSample> = series
        .samples
        .into_iter()
        .filter_map(map_failed_transactions_sample)
        .collect();
    let latest = series.latest.and_then(map_failed_transactions_sample);

    FailedTransactionsSeriesSnapshot {
        name: series.name,
        interval_secs: series.interval_secs,
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
    let payload: FailedTransactionsPayload = decode_payload(&sample, "failed-transactions-rate")?;

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

/// Response for EMA metrics endpoints (/metrics/{s2|s5|m1|m5|m15}).
///
/// This matches the MockMCP Authority API specification for metrics endpoints.
/// The EMA window affects how quickly the metrics respond to recent activity:
/// - `s2`: 2-second window - ultra-fast response for real-time monitoring
/// - `s5`: 5-second window - fastest response, best for quick demos
/// - `m1`: 1-minute window - good for short-term monitoring
/// - `m5`: 5-minute window - balanced view for medium-term simulations
/// - `m15`: 15-minute window - smoothest view for long-term trends
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
struct EmaMetricsResponse {
    /// Total number of created accounts.
    accounts: u64,
    /// EMA of accounts that recently sent funds.
    sending_accounts: u64,
    /// Transactions per second using the specified EMA window.
    #[serde(rename = "TPS")]
    tps: f64,
    /// Peak TPS observed within the EMA window.
    #[serde(rename = "PeakTPS")]
    peak_tps: f64,
    /// Timestamp (ms) when peak TPS occurred within the window.
    #[serde(rename = "PeakTPSAtMs")]
    peak_tps_at_ms: Option<i64>,
    /// EMA tokens sent per second.
    tokens_per_second: f64,
    /// Total disclosure events fired.
    total_disclosure_events: u64,
    /// Sum of all wallet balances.
    total_tokens_in_wallets: u64,
    /// Total transactions ever processed.
    total_transactions: u64,
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
    latest: Option<TotalTransactionsSample>,
    samples: Vec<TotalTransactionsSample>,
}

#[derive(Serialize, ToSchema, Clone)]
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
struct TpsPeakResponse {
    /// The maximum TPS observed in the window.
    peak_tps: Option<f64>,
    /// Timestamp (ms) when the peak TPS occurred.
    peak_at_ms: Option<i64>,
    /// The window size used for the search (ms).
    window_ms: i64,
    /// Whether this result was served from cache.
    from_cache: bool,
}

#[derive(Serialize, ToSchema)]
struct TpsSeriesSnapshot {
    name: String,
    interval_secs: u64,
    latest: Option<TpsSample>,
    samples: Vec<TpsSample>,
}

#[derive(Serialize, ToSchema, Clone)]
struct TpsSample {
    recorded_at_ms: i64,
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
struct FailedTransactionsRateSeriesSnapshot {
    name: String,
    interval_secs: u64,
    latest: Option<FailedTransactionsRateSample>,
    samples: Vec<FailedTransactionsRateSample>,
}

#[derive(Serialize, ToSchema, Clone)]
struct FailedTransactionsRateSample {
    recorded_at_ms: i64,
    rate_percent: Option<f64>,
    failed_transactions: Option<u64>,
    total_transactions: Option<u64>,
    delta_ms: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct FailedTransactionsSeriesSnapshot {
    name: String,
    interval_secs: u64,
    latest: Option<FailedTransactionsSample>,
    samples: Vec<FailedTransactionsSample>,
}

#[derive(Serialize, ToSchema, Clone)]
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
struct AverageTransactionSizeHistoricSeriesSnapshot {
    name: String,
    interval_secs: u64,
    latest: Option<AverageTransactionSizeHistoricSample>,
    samples: Vec<AverageTransactionSizeHistoricSample>,
}

#[derive(Serialize, ToSchema, Clone)]
struct AverageTransactionSizeHistoricSample {
    recorded_at_ms: i64,
    average_amount: Option<f64>,
    delta_amount: Option<String>,
    delta_transactions: Option<u64>,
    delta_ms: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct AverageTransactionSizeSeriesSnapshot {
    name: String,
    interval_secs: u64,
    latest: Option<AverageTransactionSizeSample>,
    samples: Vec<AverageTransactionSizeSample>,
}

#[derive(Serialize, ToSchema, Clone)]
struct AverageTransactionSizeSample {
    recorded_at_ms: i64,
    payload: AverageTransactionSizePayload,
}

#[derive(Serialize, ToSchema)]
struct MedianTransactionSizeResponse {
    median_amount: Option<f64>,
}

#[derive(Serialize, ToSchema)]
struct MedianTransactionSizeSeriesSnapshot {
    name: String,
    bucket_seconds: u64,
    latest: Option<MedianTransactionSizeSample>,
    samples: Vec<MedianTransactionSizeSample>,
}

#[derive(Serialize, ToSchema, Clone)]
struct MedianTransactionSizeSample {
    recorded_at_ms: i64,
    median_amount: Option<f64>,
}

#[derive(Serialize, ToSchema)]
struct TokenValueSpentResponse {
    value_spent: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct TokenValueSpentHistoricSeriesSnapshot {
    name: String,
    interval_secs: u64,
    latest: Option<TokenValueSpentHistoricSample>,
    samples: Vec<TokenValueSpentHistoricSample>,
}

#[derive(Serialize, ToSchema, Clone)]
struct TokenValueSpentHistoricSample {
    recorded_at_ms: i64,
    value_spent: Option<String>,
    delta_ms: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct TokenVelocityResponse {
    token_velocity: Option<f64>,
    value_spent: Option<String>,
    total_tokens: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct TokenVelocityHistoricSeriesSnapshot {
    name: String,
    interval_secs: u64,
    latest: Option<TokenVelocityHistoricSample>,
    samples: Vec<TokenVelocityHistoricSample>,
}

#[derive(Serialize, ToSchema, Clone)]
struct TokenVelocityHistoricSample {
    recorded_at_ms: i64,
    token_velocity: Option<f64>,
    value_spent: Option<String>,
    total_tokens: Option<String>,
    delta_ms: Option<i64>,
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
        total_transactions_historic,
        tps,
        tps_historic,
        tps_peak,
        failed_transactions_rate,
        failed_transactions_rate_historic,
        average_transaction_size,
        average_transaction_size_historic,
        median_transaction_size,
        median_transaction_size_historic,
        metrics_s2,
        metrics_s5,
        metrics_m1,
        metrics_m5,
        metrics_m15,
        token_value_spent,
        token_value_spent_historic,
        token_velocity,
        token_velocity_historic
    ),
    components(schemas(
        HealthResponse,
        EmaMetricsResponse,
        TotalTransactionsResponse,
        TotalTransactionsSeriesSnapshot,
        TotalTransactionsSample,
        TpsResponse,
        TpsSeriesSnapshot,
        TpsSample,
        TpsPeakResponse,
        FailedTransactionsResponse,
        FailedTransactionsRateSeriesSnapshot,
        FailedTransactionsRateSample,
        AverageTransactionSizeResponse,
        AverageTransactionSizeHistoricSeriesSnapshot,
        AverageTransactionSizeHistoricSample,
        MedianTransactionSizeResponse,
        MedianTransactionSizeSeriesSnapshot,
        MedianTransactionSizeSample,
        TokenValueSpentResponse,
        TokenValueSpentHistoricSeriesSnapshot,
        TokenValueSpentHistoricSample,
        TokenVelocityResponse,
        TokenVelocityHistoricSeriesSnapshot,
        TokenVelocityHistoricSample
    )),
    tags(
        (name = "health", description = "Service health checks"),
        (name = "metrics", description = "Metrics endpoints"),
        (name = "ema-metrics", description = "EMA metrics endpoints (MockMCP-compatible)")
    )
)]
struct ApiDoc;

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_common_transfer_filters(sql: &str) {
        let required_filters = [
            "FROM midnight_transfer mt",
            "JOIN events ev ON ev.id = mt.event_id",
            "mt.amount IS NOT NULL",
            "mt.amount ~ '^[0-9]+$'",
            "ev.created_at >= $1",
            "ev.created_at <= $2",
        ];

        for filter in required_filters {
            assert!(
                sql.contains(filter),
                "expected SQL to contain filter: {filter}"
            );
        }
    }

    #[test]
    fn postgres_average_sql_keeps_expected_filters() {
        assert_common_transfer_filters(POSTGRES_AVERAGE_TRANSACTION_SIZE_SQL);
        assert!(POSTGRES_AVERAGE_TRANSACTION_SIZE_SQL.contains("AVG(windowed.amount)"));
        assert!(POSTGRES_AVERAGE_TRANSACTION_SIZE_SQL.contains("COUNT(*)::bigint"));
    }

    #[test]
    fn postgres_median_sql_keeps_expected_filters() {
        assert_common_transfer_filters(POSTGRES_MEDIAN_TRANSACTION_SIZE_SQL);
        assert!(POSTGRES_MEDIAN_TRANSACTION_SIZE_SQL.contains("ORDER BY windowed.amount"));
        assert!(POSTGRES_MEDIAN_TRANSACTION_SIZE_SQL.contains("percentile_cont(0.5) WITHIN GROUP"));
    }

    #[test]
    fn average_indexer_query_gate_requires_postgres_and_window() {
        use sea_orm::DatabaseBackend;

        assert!(should_query_average_from_indexer(
            DatabaseBackend::Postgres,
            Some(60_000)
        ));
        assert!(!should_query_average_from_indexer(
            DatabaseBackend::Postgres,
            None
        ));
        assert!(!should_query_average_from_indexer(
            DatabaseBackend::Sqlite,
            Some(60_000)
        ));
    }

    #[test]
    fn median_indexer_query_gate_requires_postgres() {
        use sea_orm::DatabaseBackend;

        assert!(should_query_median_from_indexer(DatabaseBackend::Postgres));
        assert!(!should_query_median_from_indexer(DatabaseBackend::Sqlite));
    }
}

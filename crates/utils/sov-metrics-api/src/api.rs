use axum::{
    extract::{Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use serde::Serialize;
use tracing::warn;
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

use crate::metrics::collectors::failed_transactions::{
    FailedTransactionsPayload, RETENTION_SECONDS as FAILED_RETENTION_SECONDS,
};
use crate::metrics::collectors::tps::{
    TpsPayload, RETENTION_SECONDS as TPS_RETENTION_SECONDS, WINDOW_SECONDS,
};
use crate::metrics::collectors::total_transactions::{
    TotalTransactionsPayload, RETENTION_SECONDS as TOTAL_TX_RETENTION_SECONDS,
};
use crate::metrics::{MetricSample, MetricSeriesSnapshot, MetricsStore};

#[derive(Clone)]
pub struct AppState {
    pub store: MetricsStore,
}

pub fn router(state: AppState) -> Router {
    let swagger_ui = Router::from(
        SwaggerUi::new("/swagger-ui").url("/api-doc/openapi.json", ApiDoc::openapi()),
    )
    .layer(middleware::from_fn(swagger_ui_redirect));

    Router::new()
        .route("/health", get(health))
        .route("/failed-transactions-rate", get(failed_transactions_rate))
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
    path = "/tps",
    responses(
        (status = 200, description = "TPS metrics", body = TpsResponse)
    ),
    tag = "metrics"
)]
async fn tps(State(state): State<AppState>) -> Json<TpsResponse> {
    let series = state.store.snapshot("tps").await.map(map_tps_series);

    Json(TpsResponse {
        series,
        window_seconds: WINDOW_SECONDS,
        retention_seconds: TPS_RETENTION_SECONDS,
    })
}

#[utoipa::path(
    get,
    path = "/total-transactions",
    responses(
        (status = 200, description = "Completed transaction totals", body = TotalTransactionsResponse)
    ),
    tag = "metrics"
)]
async fn total_transactions(State(state): State<AppState>) -> Json<TotalTransactionsResponse> {
    let series = state
        .store
        .snapshot("total_transactions")
        .await
        .map(map_total_transactions_series);

    Json(TotalTransactionsResponse {
        series,
        retention_seconds: TOTAL_TX_RETENTION_SECONDS,
    })
}

#[utoipa::path(
    get,
    path = "/failed-transactions-rate",
    responses(
        (status = 200, description = "Rejected transaction rate", body = FailedTransactionsResponse)
    ),
    tag = "metrics"
)]
async fn failed_transactions_rate(
    State(state): State<AppState>,
) -> Json<FailedTransactionsResponse> {
    let series = state
        .store
        .snapshot("failed-transactions-rate")
        .await
        .map(map_failed_transactions_series);

    Json(FailedTransactionsResponse {
        series,
        retention_seconds: FAILED_RETENTION_SECONDS,
    })
}

fn map_tps_series(series: MetricSeriesSnapshot) -> TpsSeriesSnapshot {
    let samples: Vec<TpsSample> = series
        .samples
        .into_iter()
        .filter_map(map_tps_sample)
        .collect();
    let latest = series.latest.and_then(map_tps_sample);

    TpsSeriesSnapshot {
        name: series.name,
        interval_secs: series.interval_secs,
        max_samples: series.max_samples,
        latest,
        samples,
    }
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

fn map_tps_sample(sample: MetricSample) -> Option<TpsSample> {
    let payload: TpsPayload = match serde_json::from_value(sample.payload) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(error = %error, "Failed to parse TPS payload");
            return None;
        }
    };

    Some(TpsSample {
        recorded_at_ms: sample.recorded_at_ms,
        payload,
    })
}

fn map_total_transactions_sample(sample: MetricSample) -> Option<TotalTransactionsSample> {
    let payload: TotalTransactionsPayload = match serde_json::from_value(sample.payload) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(error = %error, "Failed to parse total transactions payload");
            return None;
        }
    };

    Some(TotalTransactionsSample {
        recorded_at_ms: sample.recorded_at_ms,
        payload,
    })
}

fn map_failed_transactions_sample(sample: MetricSample) -> Option<FailedTransactionsSample> {
    let payload: FailedTransactionsPayload = match serde_json::from_value(sample.payload) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(error = %error, "Failed to parse failed transactions payload");
            return None;
        }
    };

    Some(FailedTransactionsSample {
        recorded_at_ms: sample.recorded_at_ms,
        payload,
    })
}

#[derive(Serialize, ToSchema)]
struct HealthResponse {
    status: String,
}

#[derive(Serialize, ToSchema)]
struct TpsResponse {
    series: Option<TpsSeriesSnapshot>,
    window_seconds: u64,
    retention_seconds: u64,
}

#[derive(Serialize, ToSchema)]
struct TpsSeriesSnapshot {
    name: String,
    interval_secs: u64,
    max_samples: usize,
    latest: Option<TpsSample>,
    samples: Vec<TpsSample>,
}

#[derive(Serialize, ToSchema)]
struct TpsSample {
    recorded_at_ms: i64,
    payload: TpsPayload,
}

#[derive(Serialize, ToSchema)]
struct TotalTransactionsResponse {
    series: Option<TotalTransactionsSeriesSnapshot>,
    retention_seconds: u64,
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
struct FailedTransactionsResponse {
    series: Option<FailedTransactionsSeriesSnapshot>,
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

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Sovereign Metrics API",
        version = "0.1.0",
        description = "Metrics API for verifier worker DB stats."
    ),
    paths(health, tps, total_transactions, failed_transactions_rate),
    components(schemas(
        HealthResponse,
        TpsResponse,
        TpsSeriesSnapshot,
        TpsSample,
        TpsPayload,
        TotalTransactionsResponse,
        TotalTransactionsSeriesSnapshot,
        TotalTransactionsSample,
        TotalTransactionsPayload,
        FailedTransactionsResponse,
        FailedTransactionsSeriesSnapshot,
        FailedTransactionsSample,
        FailedTransactionsPayload
    )),
    tags(
        (name = "health", description = "Service health checks"),
        (name = "metrics", description = "Metrics endpoints")
    )
)]
struct ApiDoc;

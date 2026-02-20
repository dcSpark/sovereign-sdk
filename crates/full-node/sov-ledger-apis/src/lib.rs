use std::collections::HashMap;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::ops::Range;
use std::sync::Arc;

use anyhow::Context;
use axum::body::{to_bytes, Body};
use axum::extract::{Request, State, WebSocketUpgrade};
use axum::http::header::CONTENT_LENGTH;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{middleware, Extension};
use borsh::{BorshDeserialize, BorshSerialize};
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use sov_db::schema::types::{BatchNumber, EventNumber, TxNumber};
use sov_modules_api::da::Time;
pub use sov_modules_api::ApiTxEffect as TxEffect;
use sov_modules_api::{EventModuleName, RuntimeEventResponse};
use sov_rest_utils::errors::{
    self, database_error_response_500, internal_server_error_response_500, not_found_404,
};
use sov_rest_utils::{
    json_obj, preconfigured_router_layers, serve_generic_ws_subscription, ApiResult, ErrorObject,
    PageSelection, Pagination, Path, Query,
};
use sov_rollup_interface::common::{HexHash, HexString, SlotNumber};
use sov_rollup_interface::node::ledger_api::{
    AggregatedProofResponse, BatchIdAndOffset, BatchIdentifier, BatchResponse, EventIdentifier,
    FinalityStatus, IncludeChildren, ItemOrHash, LedgerStateProvider, QueryMode, SlotIdAndOffset,
    SlotIdentifier, SlotResponse, TxIdAndOffset, TxIdentifier, TxResponse,
};
use sov_rollup_interface::stf::TxReceiptContents;
use tokio::sync::{watch, Mutex};

type PathMap = Path<HashMap<String, NumberOrHash>>;
const TPS_BATCH_QUERY_CHUNK_SIZE: u64 = 10;
const LEDGER_CACHE_CAPACITY: usize = 1024;
const LEDGER_RESOLVER_CACHE_CAPACITY: usize = 4096;
const LEDGER_MAX_CACHEABLE_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Error to be returned when our bespoke path captures parser fails.
fn bad_path_error(key: &str) -> Response {
    ErrorObject {
        status: StatusCode::BAD_REQUEST,
        message: "Bad request".to_string(),
        details: json_obj!({
            "error": format!("{} is missing or invalid", key),
        }),
    }
    .into_response()
}

/// Finds a specific path component in a [`PathMap`] of type [`NumberOrHash`].
#[allow(clippy::result_large_err)]
fn get_path_item(path_map: &PathMap, key: &str) -> Result<NumberOrHash, Response> {
    if let Some(value) = path_map.get(key) {
        Ok(*value)
    } else {
        Err(bad_path_error(key))
    }
}

/// Finds a specific path component in a [`PathMap`] of type [`u64`]. Used for
/// parsing offsets.
#[allow(clippy::result_large_err)]
fn get_path_number(path_map: &PathMap, key: &str) -> Result<u64, Response> {
    if let Some(value) = path_map.get(key).and_then(|value| value.as_u64()) {
        Ok(value)
    } else {
        Err(bad_path_error(key))
    }
}

/// Use [`LedgerRoutes::axum_router`] to instantiate an [`axum::Router`] for
/// a specific [`LedgerStateProvider`].
///
/// This `struct` simply serves as a grouping of generics to reduce code
/// verbosity.
pub struct LedgerRoutes<T, B, Tx, E> {
    phantom: PhantomData<(T, B, Tx, E)>,
}

#[derive(Clone)]
pub struct LedgerState<T: LedgerStateProvider + Clone + Send + Sync + 'static> {
    pub ledger: T,
    pub shutdown_receiver: watch::Receiver<()>,
    response_cache: Arc<Mutex<ResponseLruCache>>,
    resolver_cache: Arc<Mutex<ResolverLruCache>>,
}

impl<T> LedgerState<T>
where
    T: LedgerStateProvider + Clone + Send + Sync + 'static,
{
    pub fn new(ledger: T, shutdown_receiver: watch::Receiver<()>) -> Self {
        Self {
            ledger,
            shutdown_receiver,
            response_cache: Arc::new(Mutex::new(ResponseLruCache::new(LEDGER_CACHE_CAPACITY))),
            resolver_cache: Arc::new(Mutex::new(ResolverLruCache::new(
                LEDGER_RESOLVER_CACHE_CAPACITY,
            ))),
        }
    }

    async fn clear_caches(&self) {
        self.response_cache.lock().await.clear();
        self.resolver_cache.lock().await.clear();
    }
}

impl<T, B, TxReceipt, E> LedgerRoutes<T, B, TxReceipt, E>
where
    T: LedgerStateProvider + Clone + Send + Sync + 'static,
    B: serde::Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
    TxReceipt: TxReceiptContents,
    E: EventModuleName
        + serde::Serialize
        + serde::de::DeserializeOwned
        + BorshSerialize
        + BorshDeserialize
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Returns an [`axum::Router`] that exposes ledger data.
    pub fn axum_router(
        ledger: T,
        shutdown_receiver: watch::Receiver<()>,
    ) -> axum::Router<LedgerState<T>> {
        let state = LedgerState::new(ledger, shutdown_receiver);
        Self::spawn_cache_invalidation_task(state.clone());
        let cache_layer = middleware::from_fn_with_state(
            state.response_cache.clone(),
            Self::cache_response_middleware,
        );

        let slot_cache_routes = axum::Router::new()
            .route(
                "/cache",
                get(Self::get_slot).route_layer(cache_layer.clone()),
            )
            .route(
                "/events/cache",
                get(Self::get_slot_events).route_layer(cache_layer.clone()),
            )
            .nest(
                "/batches/:batchOffset",
                axum::Router::new()
                    .route(
                        "/cache",
                        get(Self::get_batch).route_layer(cache_layer.clone()),
                    )
                    .nest(
                        "/txs/:txOffset",
                        axum::Router::new()
                            .route("/cache", get(Self::get_tx).route_layer(cache_layer.clone()))
                            .route(
                                "/events/cache",
                                get(Self::get_tx_events).route_layer(cache_layer.clone()),
                            )
                            .nest(
                                "/events/:eventOffset",
                                axum::Router::new()
                                    .route(
                                        "/cache",
                                        get(Self::get_event).route_layer(cache_layer.clone()),
                                    )
                                    .route_layer(middleware::from_fn_with_state(
                                        state.clone(),
                                        Self::resolve_event_offset_cached,
                                    )),
                            )
                            .route_layer(middleware::from_fn_with_state(
                                state.clone(),
                                Self::resolve_tx_offset_cached,
                            )),
                    )
                    .route_layer(middleware::from_fn_with_state(
                        state.clone(),
                        Self::resolve_batch_offset_cached,
                    )),
            );

        let batch_cache_routes = axum::Router::new()
            .route(
                "/cache",
                get(Self::get_batch).route_layer(cache_layer.clone()),
            )
            .nest(
                "/txs/:txOffset",
                axum::Router::new()
                    .route("/cache", get(Self::get_tx).route_layer(cache_layer.clone()))
                    .route(
                        "/events/cache",
                        get(Self::get_tx_events).route_layer(cache_layer.clone()),
                    )
                    .nest(
                        "/events/:eventOffset",
                        axum::Router::new()
                            .route(
                                "/cache",
                                get(Self::get_event).route_layer(cache_layer.clone()),
                            )
                            .route_layer(middleware::from_fn_with_state(
                                state.clone(),
                                Self::resolve_event_offset_cached,
                            )),
                    )
                    .route_layer(middleware::from_fn_with_state(
                        state.clone(),
                        Self::resolve_tx_offset_cached,
                    )),
            );

        let tx_cache_routes = axum::Router::new()
            .route("/cache", get(Self::get_tx).route_layer(cache_layer.clone()))
            .route(
                "/events/cache",
                get(Self::get_tx_events).route_layer(cache_layer.clone()),
            )
            .nest(
                "/events/:eventOffset",
                axum::Router::new()
                    .route(
                        "/cache",
                        get(Self::get_event).route_layer(cache_layer.clone()),
                    )
                    .route_layer(middleware::from_fn_with_state(
                        state.clone(),
                        Self::resolve_event_offset_cached,
                    )),
            );

        let event_cache_routes = axum::Router::new().route(
            "/cache",
            get(Self::get_event).route_layer(cache_layer.clone()),
        );
        let tps_cache_routes =
            axum::Router::new().route("/cache", get(Self::get_slot_tps).route_layer(cache_layer));

        let routes = axum::Router::<LedgerState<T>>::new()
            .route(
                "/aggregated-proofs/latest",
                get(Self::get_latest_aggregated_proof),
            )
            .route(
                "/aggregated-proofs/latest/ws",
                get(Self::subscribe_to_aggregated_proofs),
            )
            .route("/slots/latest/ws", get(Self::subscribe_to_head))
            .route("/slots/finalized/ws", get(Self::subscribe_to_finalized))
            .route(
                "/slots/latest/events/ws",
                get(Self::subscribe_to_slot_events),
            )
            .nest(
                "/slots/latest",
                Self::router_slot(state.clone()).route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_latest_slot,
                )),
            )
            .nest(
                "/slots/finalized",
                Self::router_slot(state.clone()).route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_finalized_slot,
                )),
            )
            .nest(
                "/slots/:slotId",
                Self::router_slot(state.clone()).route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_slot_id,
                )),
            )
            .nest(
                "/slots/:slotId",
                slot_cache_routes.route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_slot_id_cached,
                )),
            )
            .nest(
                "/tps/latest",
                Self::router_tps().route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_latest_slot,
                )),
            )
            .nest(
                "/tps/:slotId",
                Self::router_tps().route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_slot_id,
                )),
            )
            .nest(
                "/tps/:slotId",
                tps_cache_routes.route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_slot_id_cached,
                )),
            )
            .nest(
                "/batches/:batchId",
                Self::router_batch(state.clone()).route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_batch_id,
                )),
            )
            .nest(
                "/batches/:batchId",
                batch_cache_routes.route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_batch_id_cached,
                )),
            )
            .nest(
                "/txs/:txId",
                Self::router_tx(state.clone()).route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_tx_id,
                )),
            )
            .nest(
                "/txs/:txId",
                tx_cache_routes.route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_tx_id_cached,
                )),
            )
            .route("/events", get(Self::list_events))
            .route("/events/latest", get(Self::get_latest_event))
            .nest(
                "/events/:eventId",
                Self::router_event().route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_event_id,
                )),
            )
            .nest(
                "/events/:eventId",
                event_cache_routes.route_layer(middleware::from_fn_with_state(
                    state,
                    Self::resolve_event_id_cached,
                )),
            );
        preconfigured_router_layers(axum::Router::<LedgerState<T>>::new().nest("/ledger", routes))
    }

    fn spawn_cache_invalidation_task(state: LedgerState<T>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("No Tokio runtime available; cache invalidation task is disabled");
            return;
        };

        handle.spawn(async move {
            let mut shutdown = state.shutdown_receiver.clone();
            let mut slot_updates = state.ledger.subscribe_slots();
            let mut finalized_updates = state.ledger.subscribe_finalized_slots();

            loop {
                tokio::select! {
                    _ = shutdown.changed() => break,
                    maybe_slot = slot_updates.next() => {
                        if maybe_slot.is_none() {
                            break;
                        }
                        state.clear_caches().await;
                    }
                    maybe_finalized = finalized_updates.next() => {
                        if maybe_finalized.is_none() {
                            break;
                        }
                        state.clear_caches().await;
                    }
                }
            }
        });
    }

    // ROUTERS
    // -------
    // The following routers are not the typical routers that you'd find in
    // Axum examples. This is because they compose with each other through
    // nesting and layering to offer easy access to sub-resources using
    // nested routes, e.g.:
    //
    // - /slots/latest
    // - /slots/latest/batches/2
    // - /txs/0x1337/events/42

    fn router_slot(state: LedgerState<T>) -> axum::Router<LedgerState<T>> {
        axum::Router::new()
            .route("/", get(Self::get_slot))
            .nest(
                "/batches/:batchOffset",
                Self::router_batch(state.clone()).layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_batch_offset,
                )),
            )
            .route("/events", get(Self::get_slot_events))
    }

    fn router_batch(state: LedgerState<T>) -> axum::Router<LedgerState<T>> {
        axum::Router::new().route("/", get(Self::get_batch)).nest(
            "/txs/:txOffset",
            Self::router_tx(state.clone()).layer(middleware::from_fn_with_state(
                state.clone(),
                Self::resolve_tx_offset,
            )),
        )
    }

    fn router_tx(state: LedgerState<T>) -> axum::Router<LedgerState<T>> {
        axum::Router::new()
            .route("/", get(Self::get_tx))
            .route("/events", get(Self::get_tx_events))
            .nest(
                "/events/:eventOffset",
                Self::router_event().layer(middleware::from_fn_with_state(
                    state.clone(),
                    Self::resolve_event_offset,
                )),
            )
    }

    fn router_event() -> axum::Router<LedgerState<T>> {
        axum::Router::new().route("/", get(Self::get_event))
    }

    fn router_tps() -> axum::Router<LedgerState<T>> {
        axum::Router::new().route("/", get(Self::get_slot_tps))
    }

    async fn cache_response_middleware(
        State(cache): State<Arc<Mutex<ResponseLruCache>>>,
        request: Request,
        next: Next,
    ) -> Response {
        let key = request
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str().to_owned())
            .unwrap_or_else(|| request.uri().path().to_owned());

        if let Some(cached_response) = cache.lock().await.get(&key) {
            return cached_response.into_response();
        }

        let response = next.run(request).await;
        if !response.status().is_success() {
            return response;
        }

        let (parts, body) = response.into_parts();
        let content_length = parts
            .headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok());

        if let Some(length) = content_length {
            if length > LEDGER_MAX_CACHEABLE_RESPONSE_BYTES {
                tracing::warn!(
                    path = %key,
                    response_size_bytes = length,
                    max_cacheable_bytes = LEDGER_MAX_CACHEABLE_RESPONSE_BYTES,
                    "Skipping cache write: response body exceeds max cacheable size"
                );
                return Response::from_parts(parts, body);
            }
        } else {
            // Keep KISS: cache only responses with known lengths within strict bounds.
            return Response::from_parts(parts, body);
        }

        let status = parts.status;
        let headers = parts.headers;
        let body_bytes = match to_bytes(body, LEDGER_MAX_CACHEABLE_RESPONSE_BYTES).await {
            Ok(bytes) => bytes.to_vec(),
            Err(err) => {
                tracing::warn!(
                    path = %key,
                    error = %err,
                    max_cacheable_bytes = LEDGER_MAX_CACHEABLE_RESPONSE_BYTES,
                    "Skipping cache write due to unreadable or oversized response body"
                );
                return internal_server_error_response_500(
                    "Failed to read response body for cache",
                );
            }
        };

        let response_to_return = CachedResponse {
            status,
            headers,
            body: body_bytes,
        };

        if should_cache_response_body(&response_to_return.body) {
            cache.lock().await.insert(key, response_to_return.clone());
        }

        response_to_return.into_response()
    }

    // HANDLERS
    // --------
    // Most of these handlers rely on "extension" values set by the
    // middleware functions at different nesting levels. You'll need to
    // carefully inspect the routers to see how they are set.

    async fn get_slot(
        State(state): State<LedgerState<T>>,
        include_children_opt: Option<Query<IncludeChildren>>,
        Extension(slot_number): Extension<SlotNumber>,
    ) -> ApiResult<Slot<B, TxReceipt, E>> {
        match state
            .ledger
            .get_slot_by_number::<B, TxReceipt, RuntimeEventResponse<E>>(
                slot_number,
                include_children_opt.map(|q| q.0).unwrap_or_default().into(),
            )
            .await
        {
            Ok(Some(slot_response)) => Ok(Slot::new(slot_response).into()),
            Ok(None) => Err(errors::not_found_404("Slot", slot_number)),
            Err(err) => Err(errors::database_error_response_500(err)),
        }
    }

    async fn get_slot_tps(
        State(state): State<LedgerState<T>>,
        Extension(slot_number): Extension<SlotNumber>,
    ) -> ApiResult<SlotTps> {
        let slot = state
            .ledger
            .get_slot_by_number::<B, TxReceipt, RuntimeEventResponse<E>>(
                slot_number,
                QueryMode::Compact,
            )
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Slot", slot_number.get()))?;

        let previous_slot_number = slot_number.checked_sub(1).ok_or_else(|| {
            errors::bad_request_400(
                "Cannot calculate TPS for genesis slot",
                format!("Slot {} has no predecessor", slot_number.get()),
            )
        })?;

        let previous_slot = state
            .ledger
            .get_slot_by_number::<B, TxReceipt, RuntimeEventResponse<E>>(
                previous_slot_number,
                QueryMode::Compact,
            )
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Slot", previous_slot_number.get()))?;

        let block_time_ms = slot
            .timestamp
            .as_millis()
            .checked_sub(previous_slot.timestamp.as_millis())
            .ok_or_else(|| {
                errors::bad_request_400(
                    "Cannot calculate TPS when slot timestamp is earlier than its predecessor",
                    format!(
                        "slot_timestamp_ms={} predecessor_timestamp_ms={}",
                        slot.timestamp.as_millis(),
                        previous_slot.timestamp.as_millis(),
                    ),
                )
            })?;
        if block_time_ms == 0 {
            return Err(errors::bad_request_400(
                "Cannot calculate TPS when slot time delta is zero",
                format!("slot_number={}", slot_number.get()),
            ));
        }
        let block_time_ms = u64::try_from(block_time_ms)
            .map_err(|_| internal_server_error_response_500("slot timestamp delta exceeded u64"))?;

        let tx_count =
            Self::count_slot_transactions(&state.ledger, slot_number, slot.batch_range.clone())
                .await?;
        let tps = tx_count as f64 / (block_time_ms as f64 / 1000.0);

        Ok(SlotTps {
            slot_number: slot.number,
            tx_count,
            block_time_ms,
            tps,
            finality_status: slot.finality_status,
            timestamp: slot.timestamp,
        }
        .into())
    }

    async fn count_slot_transactions(
        ledger: &T,
        slot_number: SlotNumber,
        batch_range: Range<u64>,
    ) -> Result<u64, Response> {
        let mut tx_count = 0u64;
        let mut chunk_start = batch_range.start;

        while chunk_start < batch_range.end {
            let chunk_end = chunk_start
                .saturating_add(TPS_BATCH_QUERY_CHUNK_SIZE)
                .min(batch_range.end);
            let batch_ids = (chunk_start..chunk_end)
                .map(BatchIdentifier::Number)
                .collect::<Vec<_>>();
            let batches = ledger
                .get_batches::<B, TxReceipt, RuntimeEventResponse<E>>(
                    batch_ids.as_slice(),
                    QueryMode::Compact,
                )
                .await
                .map_err(database_error_response_500)?;

            for (offset, maybe_batch) in batches.into_iter().enumerate() {
                let batch_number = chunk_start + offset as u64;
                let batch = maybe_batch.ok_or_else(|| {
                    internal_server_error_response_500(format!(
                        "Missing batch {} while calculating TPS for slot {}",
                        batch_number,
                        slot_number.get()
                    ))
                })?;

                let batch_tx_count = batch
                    .tx_range
                    .end
                    .checked_sub(batch.tx_range.start)
                    .ok_or_else(|| {
                        internal_server_error_response_500(format!(
                            "Invalid tx range in batch {} while calculating TPS",
                            batch_number
                        ))
                    })?;
                tx_count = tx_count.checked_add(batch_tx_count).ok_or_else(|| {
                    internal_server_error_response_500(format!(
                        "Transaction count overflow while calculating TPS for slot {}",
                        slot_number.get()
                    ))
                })?;
            }

            chunk_start = chunk_end;
        }

        Ok(tx_count)
    }

    async fn get_slot_events(
        State(state): State<LedgerState<T>>,
        Extension(slot_number): Extension<SlotNumber>,
        event_key_prefix_opt: Option<Query<EventFilter>>,
    ) -> ApiResult<Vec<RuntimeEventResponse<E>>> {
        let filter = event_key_prefix_opt.map(|q| q.0.prefix.into());
        let events = state
            .ledger
            .get_filtered_slot_events::<B, TxReceipt, RuntimeEventResponse<E>>(
                &SlotIdentifier::Number(slot_number),
                filter,
            )
            .await
            .map_err(database_error_response_500)?;

        Ok(events.into())
    }

    async fn get_batch(
        State(state): State<LedgerState<T>>,
        include_children_opt: Option<Query<IncludeChildren>>,
        Extension(BatchNumber(batch_number)): Extension<BatchNumber>,
    ) -> ApiResult<Batch<B, TxReceipt, E>> {
        match state
            .ledger
            .get_batch_by_number::<B, TxReceipt, RuntimeEventResponse<E>>(
                batch_number,
                include_children_opt.map(|q| q.0).unwrap_or_default().into(),
            )
            .await
        {
            Ok(Some(batch_response)) => Ok(Batch::new(batch_response, batch_number).into()),
            Ok(None) => Err(errors::not_found_404("Batch", batch_number)),
            Err(err) => Err(errors::database_error_response_500(err)),
        }
    }

    async fn get_tx(
        State(state): State<LedgerState<T>>,
        include_children_opt: Option<Query<IncludeChildren>>,
        Extension(TxNumber(tx_number)): Extension<TxNumber>,
    ) -> ApiResult<Transaction<TxReceipt, E>> {
        match state
            .ledger
            .get_tx_by_number::<TxReceipt, RuntimeEventResponse<E>>(
                tx_number,
                include_children_opt.map(|q| q.0).unwrap_or_default().into(),
            )
            .await
        {
            Ok(Some(tx_response)) => Ok(Transaction::new(tx_response, tx_number).into()),
            Ok(None) => Err(errors::not_found_404("Transaction", tx_number)),
            Err(err) => Err(errors::database_error_response_500(err)),
        }
    }

    async fn get_tx_events(
        State(state): State<LedgerState<T>>,
        Extension(TxNumber(tx_number)): Extension<TxNumber>,
        event_key_prefix_opt: Option<Query<EventFilter>>,
    ) -> ApiResult<Vec<RuntimeEventResponse<E>>> {
        match state
            .ledger
            .get_events_by_txn_number::<RuntimeEventResponse<E>>(tx_number)
            .await
        {
            Ok(events) => Ok(events
                .into_iter()
                .filter(|event| {
                    if let Some(prefix) = &event_key_prefix_opt {
                        event.key.starts_with(&prefix.prefix)
                    } else {
                        true
                    }
                })
                .collect::<Vec<_>>()
                .into()),
            Err(err) => Err(errors::database_error_response_500(err)),
        }
    }

    async fn get_event(
        State(state): State<LedgerState<T>>,
        Extension(EventNumber(event_number)): Extension<EventNumber>,
    ) -> ApiResult<RuntimeEventResponse<E>> {
        match state
            .ledger
            .get_event_by_number::<RuntimeEventResponse<E>>(event_number)
            .await
        {
            Ok(Some(event_response)) => Ok(event_response.into()),
            Ok(None) => Err(errors::not_found_404("Event", event_number)),
            Err(err) => Err(errors::database_error_response_500(err)),
        }
    }

    // TODO: we're going to want to start using range/iters
    // when retrieving events from ledger db.
    // With that in mind we're using cursor based pagination
    // so the implementation can be updated without changing the REST API interface.
    async fn list_events(
        State(state): State<LedgerState<T>>,
        pagination_opt: Option<Query<Pagination<String>>>,
    ) -> ApiResult<Vec<RuntimeEventResponse<E>>> {
        let pagination = match pagination_opt {
            Some(Query(pagination)) => pagination,
            None => Default::default(),
        };
        let start = match pagination.selection {
            PageSelection::Next { cursor } => cursor
                .parse::<u64>()
                .map_err(|e| errors::bad_request_400("Cursor was not valid u64", e))?,
            PageSelection::First => 0,
            PageSelection::Last => return Err(errors::not_implemented_501()),
        };
        let end = start
            .checked_add(pagination.size as u64)
            .unwrap_or(u64::MAX);
        let nums = (start..=end)
            .map(EventIdentifier::Number)
            .collect::<Vec<_>>();
        let events = state
            .ledger
            .get_events::<RuntimeEventResponse<E>>(nums.as_slice())
            .await
            .map_err(errors::database_error_response_500)?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        Ok(events.into())
    }

    async fn get_latest_event(
        State(state): State<LedgerState<T>>,
    ) -> ApiResult<RuntimeEventResponse<E>> {
        let event_number = state
            .ledger
            .get_latest_event_number()
            .await
            .map_err(errors::database_error_response_500)?
            .ok_or_else(|| errors::not_found_404("Event", "latest"))?;
        let event = state
            .ledger
            .get_event_by_number::<RuntimeEventResponse<E>>(event_number)
            .await
            .map_err(errors::database_error_response_500)?
            .ok_or_else(|| errors::not_found_404("Event", event_number))?;
        Ok(event.into())
    }

    // ENTITY ID RESOLVERS
    // -------------------
    // These are middleware functions that resolve the entity ID (i.e.
    // numbers) or the "root" entity, given its hash or number.

    async fn resolve_latest_slot(
        State(state): State<LedgerState<T>>,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let latest_slot = state
            .ledger
            .get_head_slot_number()
            .await
            .map_err(database_error_response_500)?;

        request.extensions_mut().insert(latest_slot);
        Ok(next.run(request).await)
    }

    async fn resolve_finalized_slot(
        State(state): State<LedgerState<T>>,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let finalized_slot = state
            .ledger
            .get_latest_finalized_slot_number()
            .await
            .map_err(database_error_response_500)?;

        request.extensions_mut().insert(finalized_slot);
        Ok(next.run(request).await)
    }

    async fn resolve_slot_id(
        State(state): State<LedgerState<T>>,
        path_values: PathMap,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let identifier = match get_path_item(&path_values, "slotId")? {
            NumberOrHash::Number(number) => {
                SlotIdentifier::Number(SlotNumber::new_dangerous(number))
            }
            NumberOrHash::Hash(hash) => SlotIdentifier::Hash(hash.0),
        };

        let rollup_height = state
            .ledger
            .resolve_slot_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            // TODO: 404s should *NOT* be generated with entity IDs set to
            // "unknown", it's bad UX. Unfortunately, we need the identifier to
            // implement `ToString`, and the identifier types that we're using
            // at the time of writing don't. While we could map them into
            // `NumberOrHash` just for better error messaging, it will
            // complicate the code. Once we remove JSON-RPC identifier types, we
            // can remove this workaround and do the right thing.
            .ok_or_else(|| not_found_404("Slot", "unknown"))?;

        request.extensions_mut().insert(rollup_height);
        Ok(next.run(request).await)
    }

    async fn resolve_batch_id(
        path_values: PathMap,
        State(state): State<LedgerState<T>>,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let identifier = match get_path_item(&path_values, "batchId")? {
            NumberOrHash::Number(number) => BatchIdentifier::Number(number),
            NumberOrHash::Hash(hash) => BatchIdentifier::Hash(hash.0),
        };
        let batch_number = state
            .ledger
            .resolve_batch_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Batch", "unknown"))?;

        request.extensions_mut().insert(BatchNumber(batch_number));
        Ok(next.run(request).await)
    }

    async fn resolve_tx_id(
        State(state): State<LedgerState<T>>,
        path_values: PathMap,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let identifier = match get_path_item(&path_values, "txId")? {
            NumberOrHash::Number(number) => TxIdentifier::Number(number),
            NumberOrHash::Hash(hash) => TxIdentifier::Hash(hash.0),
        };
        let tx_number = state
            .ledger
            .resolve_tx_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Transaction", "unknown"))?;

        request.extensions_mut().insert(TxNumber(tx_number));
        Ok(next.run(request).await)
    }

    async fn resolve_event_id(
        State(state): State<LedgerState<T>>,
        path_values: PathMap,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        // Events can't be resolved by hash, only by number.
        let identifier = EventIdentifier::Number(get_path_number(&path_values, "eventId")?);

        let event_number = state
            .ledger
            .resolve_event_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Event", "unknown"))?;

        request.extensions_mut().insert(EventNumber(event_number));
        Ok(next.run(request).await)
    }

    // ENTITY ID RESOLVERS BY OFFSET
    // -----------------------------
    // These are middleware functions that resolve some entity ID based on
    // the parent entity ID and the child's offset.
    //
    // No need for a resolved by offset for slots, because they have no
    // parent entity.

    async fn resolve_batch_offset(
        State(state): State<LedgerState<T>>,
        path_values: PathMap,
        Extension(slot_number): Extension<SlotNumber>,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let batch_offset = get_path_number(&path_values, "batchOffset")?;

        let identifier = BatchIdentifier::SlotIdAndOffset(SlotIdAndOffset {
            slot_id: SlotIdentifier::Number(slot_number),
            offset: batch_offset,
        });
        let batch_number = state
            .ledger
            .resolve_batch_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Batch", batch_offset))?;

        request.extensions_mut().insert(BatchNumber(batch_number));
        Ok(next.run(request).await)
    }

    async fn resolve_tx_offset(
        State(state): State<LedgerState<T>>,
        path_values: PathMap,
        Extension(batch_number): Extension<BatchNumber>,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let tx_offset = get_path_number(&path_values, "txOffset")?;
        let identifier = TxIdentifier::BatchIdAndOffset(BatchIdAndOffset {
            batch_id: BatchIdentifier::Number(batch_number.0),
            offset: tx_offset,
        });

        let tx_number = state
            .ledger
            .resolve_tx_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Transaction", tx_offset))?;

        request.extensions_mut().insert(TxNumber(tx_number));
        Ok(next.run(request).await)
    }

    async fn resolve_event_offset(
        State(state): State<LedgerState<T>>,
        path_values: PathMap,
        Extension(tx_number): Extension<TxNumber>,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let event_offset = get_path_number(&path_values, "eventOffset")?;
        let identifier = EventIdentifier::TxIdAndOffset(TxIdAndOffset {
            tx_id: TxIdentifier::Number(tx_number.0),
            offset: event_offset,
        });
        let event_number = state
            .ledger
            .resolve_event_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Event", event_offset))?;

        request.extensions_mut().insert(EventNumber(event_number));
        Ok(next.run(request).await)
    }

    async fn resolve_slot_id_cached(
        State(state): State<LedgerState<T>>,
        path_values: PathMap,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let slot_id = get_path_item(&path_values, "slotId")?;
        let cache_key = format!("slot-id:{slot_id}");

        if let Some(cached_slot_number) = state.resolver_cache.lock().await.get(&cache_key) {
            request
                .extensions_mut()
                .insert(SlotNumber::new_dangerous(cached_slot_number));
            return Ok(next.run(request).await);
        }

        let identifier = match slot_id {
            NumberOrHash::Number(number) => {
                SlotIdentifier::Number(SlotNumber::new_dangerous(number))
            }
            NumberOrHash::Hash(hash) => SlotIdentifier::Hash(hash.0),
        };

        let slot_number = state
            .ledger
            .resolve_slot_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Slot", "unknown"))?;

        state
            .resolver_cache
            .lock()
            .await
            .insert(cache_key, slot_number.get());
        request.extensions_mut().insert(slot_number);
        Ok(next.run(request).await)
    }

    async fn resolve_batch_id_cached(
        path_values: PathMap,
        State(state): State<LedgerState<T>>,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let batch_id = get_path_item(&path_values, "batchId")?;
        let cache_key = format!("batch-id:{batch_id}");

        if let Some(cached_batch_number) = state.resolver_cache.lock().await.get(&cache_key) {
            request
                .extensions_mut()
                .insert(BatchNumber(cached_batch_number));
            return Ok(next.run(request).await);
        }

        let identifier = match batch_id {
            NumberOrHash::Number(number) => BatchIdentifier::Number(number),
            NumberOrHash::Hash(hash) => BatchIdentifier::Hash(hash.0),
        };

        let batch_number = state
            .ledger
            .resolve_batch_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Batch", "unknown"))?;

        state
            .resolver_cache
            .lock()
            .await
            .insert(cache_key, batch_number);
        request.extensions_mut().insert(BatchNumber(batch_number));
        Ok(next.run(request).await)
    }

    async fn resolve_tx_id_cached(
        State(state): State<LedgerState<T>>,
        path_values: PathMap,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let tx_id = get_path_item(&path_values, "txId")?;
        let cache_key = format!("tx-id:{tx_id}");

        if let Some(cached_tx_number) = state.resolver_cache.lock().await.get(&cache_key) {
            request.extensions_mut().insert(TxNumber(cached_tx_number));
            return Ok(next.run(request).await);
        }

        let identifier = match tx_id {
            NumberOrHash::Number(number) => TxIdentifier::Number(number),
            NumberOrHash::Hash(hash) => TxIdentifier::Hash(hash.0),
        };

        let tx_number = state
            .ledger
            .resolve_tx_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Transaction", "unknown"))?;

        state
            .resolver_cache
            .lock()
            .await
            .insert(cache_key, tx_number);
        request.extensions_mut().insert(TxNumber(tx_number));
        Ok(next.run(request).await)
    }

    async fn resolve_event_id_cached(
        State(state): State<LedgerState<T>>,
        path_values: PathMap,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let event_id = get_path_number(&path_values, "eventId")?;
        let cache_key = format!("event-id:{event_id}");

        if let Some(cached_event_number) = state.resolver_cache.lock().await.get(&cache_key) {
            request
                .extensions_mut()
                .insert(EventNumber(cached_event_number));
            return Ok(next.run(request).await);
        }

        let identifier = EventIdentifier::Number(event_id);
        let event_number = state
            .ledger
            .resolve_event_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Event", "unknown"))?;

        state
            .resolver_cache
            .lock()
            .await
            .insert(cache_key, event_number);
        request.extensions_mut().insert(EventNumber(event_number));
        Ok(next.run(request).await)
    }

    async fn resolve_batch_offset_cached(
        State(state): State<LedgerState<T>>,
        path_values: PathMap,
        Extension(slot_number): Extension<SlotNumber>,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let batch_offset = get_path_number(&path_values, "batchOffset")?;
        let cache_key = format!("batch-offset:{}:{batch_offset}", slot_number.get());

        if let Some(cached_batch_number) = state.resolver_cache.lock().await.get(&cache_key) {
            request
                .extensions_mut()
                .insert(BatchNumber(cached_batch_number));
            return Ok(next.run(request).await);
        }

        let identifier = BatchIdentifier::SlotIdAndOffset(SlotIdAndOffset {
            slot_id: SlotIdentifier::Number(slot_number),
            offset: batch_offset,
        });
        let batch_number = state
            .ledger
            .resolve_batch_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Batch", batch_offset))?;

        state
            .resolver_cache
            .lock()
            .await
            .insert(cache_key, batch_number);
        request.extensions_mut().insert(BatchNumber(batch_number));
        Ok(next.run(request).await)
    }

    async fn resolve_tx_offset_cached(
        State(state): State<LedgerState<T>>,
        path_values: PathMap,
        Extension(batch_number): Extension<BatchNumber>,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let tx_offset = get_path_number(&path_values, "txOffset")?;
        let cache_key = format!("tx-offset:{}:{tx_offset}", batch_number.0);

        if let Some(cached_tx_number) = state.resolver_cache.lock().await.get(&cache_key) {
            request.extensions_mut().insert(TxNumber(cached_tx_number));
            return Ok(next.run(request).await);
        }

        let identifier = TxIdentifier::BatchIdAndOffset(BatchIdAndOffset {
            batch_id: BatchIdentifier::Number(batch_number.0),
            offset: tx_offset,
        });

        let tx_number = state
            .ledger
            .resolve_tx_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Transaction", tx_offset))?;

        state
            .resolver_cache
            .lock()
            .await
            .insert(cache_key, tx_number);
        request.extensions_mut().insert(TxNumber(tx_number));
        Ok(next.run(request).await)
    }

    async fn resolve_event_offset_cached(
        State(state): State<LedgerState<T>>,
        path_values: PathMap,
        Extension(tx_number): Extension<TxNumber>,
        mut request: Request,
        next: Next,
    ) -> Result<Response, Response> {
        let event_offset = get_path_number(&path_values, "eventOffset")?;
        let cache_key = format!("event-offset:{}:{event_offset}", tx_number.0);

        if let Some(cached_event_number) = state.resolver_cache.lock().await.get(&cache_key) {
            request
                .extensions_mut()
                .insert(EventNumber(cached_event_number));
            return Ok(next.run(request).await);
        }

        let identifier = EventIdentifier::TxIdAndOffset(TxIdAndOffset {
            tx_id: TxIdentifier::Number(tx_number.0),
            offset: event_offset,
        });
        let event_number = state
            .ledger
            .resolve_event_identifier(&identifier)
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Event", event_offset))?;

        state
            .resolver_cache
            .lock()
            .await
            .insert(cache_key, event_number);
        request.extensions_mut().insert(EventNumber(event_number));
        Ok(next.run(request).await)
    }

    async fn get_latest_aggregated_proof(
        State(state): State<LedgerState<T>>,
    ) -> ApiResult<AggregatedProof> {
        let latest_proof: AggregatedProof = state
            .ledger
            .get_latest_aggregated_proof()
            .await
            .map_err(database_error_response_500)?
            .ok_or_else(|| not_found_404("Aggregated proof", "latest"))?
            .try_into()
            .map_err(internal_server_error_response_500)?;

        Ok(latest_proof.into())
    }

    // SUBSCRIPTIONS
    // -------------

    async fn subscribe_to_slot_events(
        State(state): State<LedgerState<T>>,
        event_key_prefix_opt: Option<Query<EventFilter>>,
        ws: WebSocketUpgrade,
    ) -> impl IntoResponse {
        ws.on_upgrade(|socket| async move {
            let subscription = state
                .ledger
                .subscribe_slots()
                .then(|slot_num| {
                    let ledger = state.ledger.clone();
                    let filter = event_key_prefix_opt
                        .as_ref()
                        .map(|q| q.0.prefix.as_bytes().to_vec());
                    async move {
                        let Ok(events) = ledger
                            .get_filtered_slot_events::<B, TxReceipt, RuntimeEventResponse<E>>(
                                &SlotIdentifier::Number(slot_num),
                                filter,
                            )
                            .await
                        else {
                            anyhow::bail!("Error fetching events for slot {}", slot_num);
                        };

                        Ok(SlotEvents {
                            rollup_height: slot_num.get(),
                            events,
                        })
                    }
                })
                .boxed();

            serve_generic_ws_subscription(socket, subscription, state.shutdown_receiver).await;
        })
    }

    async fn subscribe_to_aggregated_proofs(
        State(state): State<LedgerState<T>>,
        ws: WebSocketUpgrade,
    ) -> impl IntoResponse {
        ws.on_upgrade(|socket| async move {
            let subscription = state.ledger.subscribe_proof_saved().map(|data| {
                AggregatedProof::try_from(data)
                    .context("Failed to convert proof to REST API representation")
            });
            serve_generic_ws_subscription(socket, subscription, state.shutdown_receiver).await;
        })
    }

    async fn subscribe_to_head(
        State(state): State<LedgerState<T>>,
        maybe_query: Option<Query<IncludeChildren>>,
        ws: WebSocketUpgrade,
    ) -> impl IntoResponse {
        let query_mode: QueryMode = if let Some(mode) = maybe_query {
            mode.0.into()
        } else {
            QueryMode::Compact
        };
        let ledger = state.ledger;

        ws.on_upgrade(move |socket| async move {
            let subscription = ledger
                .subscribe_slots()
                .then(|slot_num| {
                    let ledger = ledger.clone();
                    async move {
                        let Ok(Some(slot)) = ledger
                            .get_slot_by_number::<B, TxReceipt, RuntimeEventResponse<E>>(
                                slot_num, query_mode,
                            )
                            .await
                        else {
                            anyhow::bail!("Slot with number {} does not exist", slot_num);
                        };
                        Ok(Slot::<B, TxReceipt, E>::new(slot))
                    }
                })
                .boxed();

            serve_generic_ws_subscription(socket, subscription, state.shutdown_receiver).await;
        })
    }

    async fn subscribe_to_finalized(
        State(state): State<LedgerState<T>>,
        maybe_query: Option<Query<IncludeChildren>>,
        ws: WebSocketUpgrade,
    ) -> impl IntoResponse {
        let query_mode: QueryMode = if let Some(mode) = maybe_query {
            mode.0.into()
        } else {
            QueryMode::Compact
        };
        let ledger = state.ledger;

        ws.on_upgrade(move |socket| async move {
            let initial_slot = match ledger.get_latest_finalized_slot_number().await {
                Ok(s) => s,
                Err(error) => {
                    // There always should be the latest finalized slot number,
                    // Unless there's a problem with storage.
                    tracing::error!(
                        error = error.to_string(),
                        "Error fetch latest finalized slot number"
                    );
                    return;
                }
            };

            let subscription = ledger
                .subscribe_finalized_slots()
                .scan(
                    initial_slot,
                    move |last_notified_slot, incoming_slot_num| {
                        tracing::trace!(
                            %last_notified_slot,
                            %incoming_slot_num,
                            "Incoming finalized slot number in WebSocket handler"
                        );
                        let old_last = *last_notified_slot;
                        // Not ideal, since slot results with an error won't get re-notified.
                        // An incoming slot is going to be included in the notification.
                        *last_notified_slot = incoming_slot_num.saturating_add(1);

                        let ledger = ledger.clone();
                        async move {
                            let capacity =
                                incoming_slot_num.saturating_sub(old_last.get()).get() as usize;
                            let mut slots = Vec::with_capacity(capacity);
                            tracing::trace!(
                                from = %old_last,
                                up_to_inc = %incoming_slot_num,
                                "Going to notify about finalized slots"
                            );
                            for slot_number in old_last.range_inclusive(incoming_slot_num) {
                                let slot_result = match ledger
                                    .get_slot_by_number::<B, TxReceipt, RuntimeEventResponse<E>>(
                                        slot_number,
                                        query_mode,
                                    )
                                    .await
                                {
                                    Ok(Some(slot)) => Ok(Slot::<B, TxReceipt, E>::new(slot)),
                                    Ok(None) => Err(anyhow::anyhow!(
                                        "Slot with number {} does not exist",
                                        slot_number
                                    )),
                                    Err(err) => Err(anyhow::anyhow!(
                                        "Failed to query slot with number: {}",
                                        err.to_string()
                                    )),
                                };
                                tracing::trace!(%slot_number, "Preparing slot result for sending to websocket");
                                slots.push(slot_result);
                            }
                            tracing::trace!(
                                from = %old_last,
                                up_to_inc = %incoming_slot_num,
                                "Collected websocket notification about finalized slots"
                            );
                            // Returning `Some(...)` yields items to the *downstream*;
                            // returning `None` would end the stream.
                            Some(futures::stream::iter(slots))
                        }
                    },
                )
                .flatten()
                .boxed();

            serve_generic_ws_subscription(socket, subscription, state.shutdown_receiver).await;
        })
    }
}

#[derive(Deserialize)]
struct EventFilter {
    prefix: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SlotEvents<E> {
    rollup_height: u64,
    events: Vec<RuntimeEventResponse<E>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename = "slotTps")]
struct SlotTps {
    pub slot_number: u64,
    pub tx_count: u64,
    pub block_time_ms: u64,
    pub tps: f64,
    pub finality_status: FinalityStatus,
    pub timestamp: Time,
}

#[serde_with::serde_as]
#[derive(
    Debug, Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, derive_more::Display,
)]
#[serde(untagged)]
enum NumberOrHash {
    Number(#[serde_as(as = "serde_with::DisplayFromStr")] u64),
    Hash(HexHash),
}

impl NumberOrHash {
    fn as_u64(&self) -> Option<u64> {
        match self {
            NumberOrHash::Number(number) => Some(*number),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename = "slot",
    bound = "B: Serialize + DeserializeOwned, TxReceipt: TxReceiptContents, E: Serialize + DeserializeOwned"
)]
struct Slot<B, TxReceipt: TxReceiptContents, E> {
    pub number: u64,
    pub hash: HexHash,
    pub state_root: HexString,
    pub batch_range: Range<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batches: Option<Vec<Batch<B, TxReceipt, E>>>,
    pub finality_status: FinalityStatus,
    pub timestamp: Time,
}

impl<B, TxReceipt: TxReceiptContents, E> Slot<B, TxReceipt, E> {
    fn new(slot: SlotResponse<B, TxReceipt, RuntimeEventResponse<E>>) -> Self {
        let batches = match slot.batches {
            Some(batches) => {
                let mut output = vec![];
                let mut failed_conversion = false;
                for (offset, batch) in batches.into_iter().enumerate() {
                    if let ItemOrHash::Full(batch) = batch {
                        output.push(Batch::new(batch, slot.batch_range.start + offset as u64));
                    } else {
                        failed_conversion = true;
                        break;
                    }
                }
                if failed_conversion {
                    None
                } else {
                    Some(output)
                }
            }
            None => None,
        };

        Self {
            number: slot.number,
            hash: HexHash::new(slot.hash),
            state_root: HexString(slot.state_root),
            batch_range: slot.batch_range,
            batches,
            finality_status: slot.finality_status,
            timestamp: slot.timestamp,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename = "batch",
    bound = "B: Serialize + DeserializeOwned, TxReceipt: TxReceiptContents, E: Serialize + DeserializeOwned"
)]
struct Batch<B, TxReceipt: TxReceiptContents, E> {
    pub number: u64,
    pub hash: HexHash,
    pub tx_range: Range<u64>,
    pub receipt: B,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub txs: Option<Vec<Transaction<TxReceipt, E>>>,
    pub slot_number: SlotNumber,
}

impl<B, TxReceipt: TxReceiptContents, E> Batch<B, TxReceipt, E> {
    fn new(batch: BatchResponse<B, TxReceipt, RuntimeEventResponse<E>>, number: u64) -> Self {
        let txs = match batch.txs {
            Some(txs) => {
                let mut output = vec![];
                let mut failed_conversion = false;
                for (offset, tx) in txs.into_iter().enumerate() {
                    if let ItemOrHash::Full(tx) = tx {
                        output.push(Transaction::new(tx, batch.tx_range.start + offset as u64));
                    } else {
                        failed_conversion = true;
                        break;
                    }
                }
                if failed_conversion {
                    None
                } else {
                    Some(output)
                }
            }
            None => None,
        };
        Self {
            number,
            hash: HexHash::new(batch.hash),
            tx_range: batch.tx_range,
            receipt: batch.receipt,
            txs,
            slot_number: batch.slot_number,
        }
    }
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename = "tx",
    bound = "TxReceipt: TxReceiptContents, E: Serialize + DeserializeOwned"
)]
struct Transaction<TxReceipt: TxReceiptContents, E> {
    pub number: u64,
    pub hash: HexHash,
    pub event_range: Range<u64>,
    #[serde_as(as = "serde_with::base64::Base64")]
    pub body: Vec<u8>,
    pub receipt: TxEffect<TxReceipt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<RuntimeEventResponse<E>>>,
    pub batch_number: u64,
}

impl<TxReceipt: TxReceiptContents, E> Transaction<TxReceipt, E> {
    fn new(tx: TxResponse<TxReceipt, RuntimeEventResponse<E>>, number: u64) -> Self {
        Self {
            number,
            hash: HexHash::new(tx.hash),
            event_range: tx.event_range,
            body: tx.body.unwrap_or_default(),
            receipt: tx.receipt.into(),
            events: tx.events,
            batch_number: tx.batch_number,
        }
    }
}

// This type supplies the JSON API representation of [`AggregatedProofResponse`].
#[serde_as]
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename = "aggregatedProof")]
struct AggregatedProof {
    #[serde_as(as = "serde_with::base64::Base64")]
    pub proof: Vec<u8>,
}

#[derive(Debug, Clone)]
struct CachedResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl CachedResponse {
    fn into_response(self) -> Response {
        let mut response = Response::new(Body::from(self.body));
        *response.status_mut() = self.status;
        *response.headers_mut() = self.headers;
        response
    }
}

fn should_cache_response_body(body: &[u8]) -> bool {
    // Guard against caching placeholder success payloads that might become real data later.
    let mut start = 0usize;
    while start < body.len() && body[start].is_ascii_whitespace() {
        start += 1;
    }
    if start == body.len() {
        return false;
    }

    let mut end = body.len();
    while end > start && body[end - 1].is_ascii_whitespace() {
        end -= 1;
    }

    let trimmed = &body[start..end];
    if trimmed.eq_ignore_ascii_case(b"null") {
        return false;
    }

    let Ok(json) = serde_json::from_slice::<serde_json::Value>(trimmed) else {
        return true;
    };

    if json.is_null() {
        return false;
    }

    if let Some(array) = json.as_array() {
        return !array.is_empty();
    }

    if let Some(object) = json.as_object() {
        if object.is_empty() {
            return false;
        }

        if let Some(status) = object
            .get("finality_status")
            .and_then(|value| value.as_str())
        {
            return status == "finalized";
        }
    }

    true
}

#[derive(Debug)]
struct ResponseLruCache {
    capacity: usize,
    entries: HashMap<String, CachedResponse>,
    access_order: VecDeque<String>,
}

impl ResponseLruCache {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "cache capacity must be greater than zero");
        Self {
            capacity,
            entries: HashMap::new(),
            access_order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &str) -> Option<CachedResponse> {
        let value = self.entries.get(key).cloned()?;
        self.touch(key);
        Some(value)
    }

    fn insert(&mut self, key: String, response: CachedResponse) {
        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), response);
            self.touch(&key);
            return;
        }

        if self.entries.len() >= self.capacity {
            if let Some(evicted_key) = self.access_order.pop_front() {
                self.entries.remove(&evicted_key);
            }
        }

        self.access_order.push_back(key.clone());
        self.entries.insert(key, response);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.access_order.clear();
    }

    fn touch(&mut self, key: &str) {
        if let Some(pos) = self
            .access_order
            .iter()
            .position(|existing| existing == key)
        {
            self.access_order.remove(pos);
        }
        self.access_order.push_back(key.to_owned());
    }
}

#[derive(Debug)]
struct ResolverLruCache {
    capacity: usize,
    entries: HashMap<String, u64>,
    access_order: VecDeque<String>,
}

impl ResolverLruCache {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "cache capacity must be greater than zero");
        Self {
            capacity,
            entries: HashMap::new(),
            access_order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &str) -> Option<u64> {
        let value = *self.entries.get(key)?;
        self.touch(key);
        Some(value)
    }

    fn insert(&mut self, key: String, value: u64) {
        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), value);
            self.touch(&key);
            return;
        }

        if self.entries.len() >= self.capacity {
            if let Some(evicted_key) = self.access_order.pop_front() {
                self.entries.remove(&evicted_key);
            }
        }

        self.access_order.push_back(key.clone());
        self.entries.insert(key, value);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.access_order.clear();
    }

    fn touch(&mut self, key: &str) {
        if let Some(pos) = self
            .access_order
            .iter()
            .position(|existing| existing == key)
        {
            self.access_order.remove(pos);
        }
        self.access_order.push_back(key.to_owned());
    }
}

impl TryFrom<AggregatedProofResponse> for AggregatedProof {
    type Error = anyhow::Error;

    fn try_from(value: AggregatedProofResponse) -> Result<Self, Self::Error> {
        let proof: Vec<u8> = value.proof.raw_aggregated_proof.to_vec();
        Ok(Self { proof })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_or_hash_to_string() {
        assert_eq!(NumberOrHash::Number(0).to_string(), "0");
        assert_eq!(
            NumberOrHash::Hash(HexHash::new([0; 32])).to_string(),
            "0x0000000000000000000000000000000000000000000000000000000000000000",
        );
    }

    #[test]
    fn response_lru_cache_evicts_least_recently_used() {
        let mut cache = ResponseLruCache::new(2);

        let first_key = "first".to_owned();
        let second_key = "second".to_owned();
        let third_key = "third".to_owned();
        let make_response = |body: u8| CachedResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: vec![body],
        };

        cache.insert(first_key.clone(), make_response(1));
        cache.insert(second_key.clone(), make_response(2));

        // Touch "first" so "second" becomes LRU.
        assert!(cache.get(&first_key).is_some());

        cache.insert(third_key.clone(), make_response(3));

        assert!(cache.get(&first_key).is_some());
        assert!(cache.get(&second_key).is_none());
        assert!(cache.get(&third_key).is_some());
    }

    #[test]
    fn resolver_lru_cache_evicts_least_recently_used() {
        let mut cache = ResolverLruCache::new(2);

        cache.insert("a".to_owned(), 1);
        cache.insert("b".to_owned(), 2);
        assert_eq!(cache.get("a"), Some(1));

        cache.insert("c".to_owned(), 3);

        assert_eq!(cache.get("a"), Some(1));
        assert_eq!(cache.get("b"), None);
        assert_eq!(cache.get("c"), Some(3));
    }

    #[test]
    fn cache_guard_rejects_empty_and_null_payloads() {
        assert!(!should_cache_response_body(b""));
        assert!(!should_cache_response_body(b"   \n\t"));
        assert!(!should_cache_response_body(b"null"));
        assert!(!should_cache_response_body(b"  null  "));
        assert!(!should_cache_response_body(b"[]"));
        assert!(!should_cache_response_body(b"{}"));
        assert!(!should_cache_response_body(
            br#"{"finality_status":"pending"}"#
        ));
        assert!(should_cache_response_body(
            br#"{"finality_status":"finalized"}"#
        ));
        assert!(should_cache_response_body(br#"{"number":1}"#));
    }
}

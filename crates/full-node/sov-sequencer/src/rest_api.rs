//! Utilities and definitions for the sequencer's REST APIs.

use std::env;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::ws::WebSocket;
use axum::extract::{ws, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::Json;
use futures::StreamExt;
#[cfg(feature = "test-utils")]
use futures::TryStreamExt;
use hex::FromHex;
use midnight_privacy::SpendPublic;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, Database, DatabaseConnection, EntityTrait,
    QueryFilter,
};
use serde_with::base64::Base64;
use serde_with::serde_as;
use sov_midnight_da::storable::worker_verified_transactions;
use sov_modules_api::capabilities::TransactionAuthenticator;
use sov_modules_api::runtime::Runtime;
use sov_modules_api::{RawTx, RuntimeEventProcessor, RuntimeEventResponse};
use sov_rest_utils::{
    errors, preconfigured_router_layers, serve_generic_ws_subscription, ApiResult, FilterQuery,
    PageSelection, PaginatedResponse, Pagination, Path, Query,
};
use sov_rollup_interface::da::{DaBlobHash, DaSpec};
use sov_rollup_interface::node::da::DaService;
use sov_rollup_interface::TxHash;
use tokio::sync::watch::Receiver;
use tokio::sync::OnceCell;
use tokio_stream::wrappers::BroadcastStream;

use crate::common::{
    error_not_fully_synced, take_sequencer_metrics, AcceptedTx, Sequencer, SequencerMetrics,
};
use crate::TxStatus;

/// Shared connection pool for the worker_txs database (worker_verified_transactions table).
static WORKER_DB: OnceCell<DatabaseConnection> = OnceCell::const_new();

/// Get a shared connection to the worker_txs database, initializing it on first use.
async fn get_worker_db() -> Result<&'static DatabaseConnection, axum::response::Response> {
    use std::time::Duration;

    let res: Result<&'static DatabaseConnection, String> = WORKER_DB
        .get_or_try_init(|| async {
            let connection_string = env::var("SOV_WORKER_TX_DB_CONNECTION_STRING")
                .map_err(|_| "SOV_WORKER_TX_DB_CONNECTION_STRING env var is not set".to_string())?;

            if connection_string.starts_with("sqlite:") {
                use sea_orm::sqlx::sqlite::{
                    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
                };
                use std::str::FromStr;

                let sqlite_opts = SqliteConnectOptions::from_str(&connection_string)
                    .map_err(|err| {
                        format!(
                            "Failed to parse worker DB SQLite connection string: {err}"
                        )
                    })?
                    // Favor write throughput for the worker_txs DB: WAL + NORMAL
                    // keeps fsync costs reasonable while retaining durability.
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Normal)
                    .busy_timeout(Duration::from_millis(30_000));

                let pool = SqlitePoolOptions::new()
                    .max_connections(5)
                    .min_connections(1)
                    .acquire_timeout(Duration::from_secs(30))
                    .idle_timeout(Some(Duration::from_secs(300)))
                    .max_lifetime(Some(Duration::from_secs(1800)))
                    .connect_with(sqlite_opts)
                    .await
                    .map_err(|err| format!("Failed to connect to worker SQLite DB: {err}"))?;

                Ok(DatabaseConnection::SqlxSqlitePoolConnection(pool.into()))
            } else {
                Database::connect(connection_string)
                    .await
                    .map_err(|err| format!("Failed to connect to worker DB: {err}"))
            }
        })
        .await;

    res.map_err(|msg| {
        if msg.contains("env var is not set") {
            errors::internal_server_error_response_500(
                "SOV_WORKER_TX_DB_CONNECTION_STRING env var is not set",
            )
        } else {
            errors::database_error_response_500(anyhow::anyhow!(msg))
        }
    })
}

/// [`StartFrom`] is used as a query parameter for the txs subscription
#[derive(
    Debug,
    Copy,
    Clone,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    derive_more::Display,
)]
#[display("{}", self.start_from)]
pub struct StartFrom {
    start_from: u64,
}

/// Provides REST APIs for any [`Sequencer`]. See [`SequencerApis::rest_api_server`].
#[derive(derivative::Derivative)]
#[derivative(Clone(bound = ""))]
pub struct SequencerApis<Seq: Sequencer> {
    sequencer: Arc<Seq>,
    shutdown_receiver: Receiver<()>,
}

impl<Seq: Sequencer> SequencerApis<Seq> {
    /// Creates a new Axum router for this sequencer.
    pub fn rest_api_server(seq: Arc<Seq>, shutdown_receiver: Receiver<()>) -> axum::Router<()> {
        let state = Self {
            sequencer: seq,
            shutdown_receiver,
        };

        let router = axum::Router::new()
            .route(
                "/sequencer/worker_txs/:tx_hash",
                axum::routing::post(Self::axum_process_worker_tx),
            )
            .route("/sequencer/txs", axum::routing::post(Self::axum_accept_tx))
            .route("/sequencer/ready", axum::routing::get(Self::axum_get_ready))
            .route(
                "/sequencer/txs/:tx_hash/status",
                axum::routing::get(Self::axum_get_tx_status),
            )
            .route(
                "/sequencer/txs/:tx_hash",
                axum::routing::get(Self::axum_get_tx),
            )
            .route(
                "/sequencer/txs/:tx_hash/ws",
                axum::routing::get(Self::axum_get_tx_ws),
            )
            .route(
                "/sequencer/events/ws",
                axum::routing::get(Self::subscribe_to_events),
            )
            .route(
                "/sequencer/txs/ws",
                axum::routing::get(Self::subscribe_to_transactions),
            )
            .route(
                "/sequencer/unstable/events/:eventId",
                axum::routing::get(Self::axum_get_event),
            )
            .route(
                "/sequencer/unstable/events",
                axum::routing::get(Self::axum_list_events),
            );

        #[cfg(feature = "test-utils")]
        let router = router
            .route(
                "/sequencer/test-utils/blobs/ws",
                axum::routing::get(Self::subscribe_to_blobs_from_blob_sender),
            )
            .route(
                "/sequencer/test-utils/force-close-batch",
                axum::routing::post(Self::axum_force_close_batch),
            )
            .route(
                "/sequencer/test-utils/state-updates/ws",
                axum::routing::get(Self::subscribe_to_state_updates_unstable),
            );

        preconfigured_router_layers(router).with_state(state)
    }

    async fn send_initial_status_to_ws(
        &self,
        tx_hash: TxHash,
        socket: &mut WebSocket,
    ) -> anyhow::Result<()> {
        // Send a message with the initial status of the transaction,
        // without waiting for it to change for the first time.
        let initial_status = self.sequencer.tx_status(&tx_hash).await?;
        let ws_msg = ws::Message::Text(serde_json::to_string(&TxInfo {
            id: tx_hash,
            status: initial_status,
        })?);
        socket.send(ws_msg).await?;

        Ok(())
    }

    async fn axum_get_tx_ws(
        state: State<Self>,
        tx_hash: Path<TxHash>,
        ws: ws::WebSocketUpgrade,
    ) -> impl IntoResponse {
        let tx_status_manager = state.sequencer.tx_status_manager().clone();

        ws.on_upgrade(move |mut socket| async move {
            let (_dropper, receiver) = tx_status_manager.subscribe(tx_hash.0);

            // After "terminal" tx status updates (i.e. after which
            // we'll no longer send any new notifications), we close the
            // connection.
            let subscription = futures::stream::unfold(
                // We use the state to keep track of whether or not the last notification
                // was terminal.
                //
                // By wrapping the `receiver` in a `BroadcastStream`, we
                // ensure it'll be dropped before `_dropper`.
                (false, BroadcastStream::new(receiver)),
                |(terminated, mut stream)| async move {
                    if terminated {
                        None
                    } else {
                        let next = stream.next().await?;
                        let is_terminal: bool = next
                            .as_ref()
                            .map(|status| status.is_terminal())
                            // Errors result in WebSocket connection termination.
                            .unwrap_or(true);
                        Some((next, (is_terminal, stream)))
                    }
                },
            )
            // Finally, convert the data into the type that we want to
            // serialize over the WS connection.
            .map(|data| {
                data.context("Failed to subscribe to tx status updates")
                    .map(|status| TxInfo {
                        id: tx_hash.0,
                        status,
                    })
            })
            .boxed();

            state
                .send_initial_status_to_ws(tx_hash.0, &mut socket)
                .await
                .ok();

            serve_generic_ws_subscription(socket, subscription, state.shutdown_receiver.clone())
                .await;
        })
    }

    async fn axum_get_ready(state: State<Self>) -> ApiResult<()> {
        match state.sequencer.is_ready().await {
            Ok(()) => Ok(().into()),
            Err(details) => Err(error_not_fully_synced(details).into_response()),
        }
    }

    async fn axum_get_tx_status(
        state: State<Self>,
        tx_hash: Path<TxHash>,
    ) -> ApiResult<TxInfo<<<Seq::Da as DaService>::Spec as DaSpec>::TransactionId>> {
        let tx_status = state.sequencer.tx_status(&tx_hash.0).await;

        if let Ok(tx_status) = tx_status {
            Ok(TxInfo {
                id: tx_hash.0,
                status: tx_status,
            }
            .into())
        } else {
            Err(errors::not_found_404("Transaction", tx_hash.0))
        }
    }

    async fn axum_get_tx(
        state: State<Self>,
        tx_hash: Path<TxHash>,
    ) -> ApiResult<ApiAcceptedTx<Seq::Confirmation>> {
        let tx = state.sequencer.get_tx(tx_hash.0).await.map_err(|e| {
            tracing::error!(error = %e, "Error getting transaction");
            errors::database_error_500("Unable to retrieve transaction").into_response()
        })?;
        if let Some(tx) = tx {
            let tx: ApiAcceptedTx<_> = tx.into();
            Ok(tx.into())
        } else {
            Err(errors::not_found_404("Transaction", tx_hash.0))
        }
    }

    /// Processes pre-verified worker transactions using an optimized path.
    ///
    /// This endpoint handles transactions that have been verified off-chain by the worker
    /// (proof verifier service). Key optimizations:
    ///
    /// 1. **Avoids reading large proof blob**: Uses the optimized pre-authenticated
    ///    `serialized_tx_base64` column (no full proof blob in DB)
    /// 2. **Uses pre-computed hash**: Reads `tx_hash` from database instead of recomputing
    /// 3. **Specialized accept path**: Calls `accept_serialized_pre_authenticated_tx` which skips
    ///    decoding the large proof blob and re-authentication since verification is already done
    ///
    /// The proof outputs are cached for the runtime to use during execution, allowing
    /// the transaction to execute without re-verifying the proof.
    async fn axum_process_worker_tx(
        State(state): State<Self>,
        Path(tx_hash): Path<String>,
    ) -> ApiResult<
        TxInfoWithConfirmation<DaBlobHash<<Seq::Da as DaService>::Spec>, Seq::Confirmation>,
    > {
        // Use a shared connection pool for the worker transactions database.
        let db = get_worker_db().await?;

        let record = worker_verified_transactions::Entity::find()
            .filter(worker_verified_transactions::Column::TxHash.eq(tx_hash.clone()))
            .one(db)
            .await
            .map_err(|err| errors::database_error_response_500(err))?;

        let Some(model) = record else {
            return Err(errors::not_found_404("Worker transaction", tx_hash));
        };

        let transaction_data: serde_json::Value = serde_json::from_str(&model.transaction_data)
            .map_err(|err| errors::bad_request_400("Invalid transaction_data JSON", err))?;

        // Parse the pre-computed tx_hash
        let tx_hash_hex = model.tx_hash.trim_start_matches("0x");
        let tx_hash_bytes = Vec::from_hex(tx_hash_hex)
            .map_err(|err| errors::bad_request_400("Invalid tx_hash in database", err))?;
        let tx_hash_array: [u8; 32] = tx_hash_bytes
            .try_into()
            .map_err(|_| errors::bad_request_400("Invalid tx_hash length", "Expected 32 bytes"))?;
        let tx_hash_value = TxHash::from(tx_hash_array);

        // Require the optimized serialized pre-auth path; do not fall back to other paths.
        let serialized_tx_base64 = match &model.serialized_tx_base64 {
            Some(v) => v,
            None => {
                return Err(errors::bad_request_400(
                    "Missing serialized_tx_base64",
                    "Worker-verified transaction must include serialized_tx_base64 for optimized sequencer path",
                ))
            }
        };

        tracing::debug!(%tx_hash_value, "Using OPTIMIZED pre-authenticated path (serialized tx, no blob, no auth)");

        // Decide intent based on parsed transaction data and proof outputs.
        enum WorkerTxIntent {
            Deposit,
            Transfer { proof_outputs: SpendPublic },
            Withdraw { proof_outputs: SpendPublic },
        }

        let worker_tx_intent = if let Some(withdraw) =
            transaction_data.get("withdraw").and_then(|v| v.as_object())
        {
            let anchor_root_hex = withdraw
                .get("anchor_root")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    errors::bad_request_400("Invalid transaction data", "Missing anchor_root")
                })?;
            let anchor_root_vec = Vec::from_hex(anchor_root_hex.trim_start_matches("0x"))
                .map_err(|err| errors::bad_request_400("Invalid anchor_root hex", err))?;
            let anchor_root_array: [u8; 32] = anchor_root_vec
                .try_into()
                .map_err(|_| errors::bad_request_400("Invalid anchor_root length", ""))?;

            let nullifier_hex = withdraw
                .get("nullifier")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    errors::bad_request_400("Invalid transaction data", "Missing nullifier")
                })?;
            let nullifier_vec = Vec::from_hex(nullifier_hex.trim_start_matches("0x"))
                .map_err(|err| errors::bad_request_400("Invalid nullifier hex", err))?;
            let nullifier_array: [u8; 32] = nullifier_vec
                .try_into()
                .map_err(|_| errors::bad_request_400("Invalid nullifier length", ""))?;

            let withdraw_amount = withdraw
                .get("withdraw_amount")
                .and_then(|v| match v {
                    serde_json::Value::String(s) => s.parse::<u128>().ok(),
                    serde_json::Value::Number(n) => n.as_u64().map(|n| n as u128),
                    _ => None,
                })
                .ok_or_else(|| {
                    errors::bad_request_400("Invalid transaction data", "Missing withdraw_amount")
                })?;

            let proof_outputs_str = model.proof_outputs.trim();
            if proof_outputs_str.is_empty() || proof_outputs_str == "{}" {
                return Err(errors::bad_request_400(
                    "Proof outputs missing",
                    "Withdraw-like transaction requires proof outputs",
                ));
            }

            let proof_outputs: SpendPublic = serde_json::from_str(proof_outputs_str)
                .map_err(|err| errors::bad_request_400("Invalid proof_outputs JSON", err))?;

            if proof_outputs.anchor_root != anchor_root_array {
                return Err(errors::bad_request_400(
                    "Proof outputs mismatch",
                    "anchor_root does not match",
                ));
            }
            if proof_outputs.nullifier != nullifier_array {
                return Err(errors::bad_request_400(
                    "Proof outputs mismatch",
                    "nullifier does not match",
                ));
            }
            if proof_outputs.withdraw_amount != withdraw_amount {
                return Err(errors::bad_request_400(
                    "Proof outputs mismatch",
                    "withdraw_amount does not match",
                ));
            }

            midnight_privacy::cache_pre_verified_spend(proof_outputs.clone());
            WorkerTxIntent::Withdraw { proof_outputs }
        } else if let Some(transfer) =
            transaction_data.get("transfer").and_then(|v| v.as_object())
        {
            let anchor_root_hex = transfer
                .get("anchor_root")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    errors::bad_request_400("Invalid transaction data", "Missing anchor_root")
                })?;
            let anchor_root_vec = Vec::from_hex(anchor_root_hex.trim_start_matches("0x"))
                .map_err(|err| errors::bad_request_400("Invalid anchor_root hex", err))?;
            let anchor_root_array: [u8; 32] = anchor_root_vec
                .try_into()
                .map_err(|_| errors::bad_request_400("Invalid anchor_root length", ""))?;

            let nullifier_hex = transfer
                .get("nullifier")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    errors::bad_request_400("Invalid transaction data", "Missing nullifier")
                })?;
            let nullifier_vec = Vec::from_hex(nullifier_hex.trim_start_matches("0x"))
                .map_err(|err| errors::bad_request_400("Invalid nullifier hex", err))?;
            let nullifier_array: [u8; 32] = nullifier_vec
                .try_into()
                .map_err(|_| errors::bad_request_400("Invalid nullifier length", ""))?;

            let proof_outputs_str = model.proof_outputs.trim();
            if proof_outputs_str.is_empty() || proof_outputs_str == "{}" {
                return Err(errors::bad_request_400(
                    "Proof outputs missing",
                    "Transfer-like transaction requires proof outputs",
                ));
            }

            let proof_outputs: SpendPublic = serde_json::from_str(proof_outputs_str)
                .map_err(|err| errors::bad_request_400("Invalid proof_outputs JSON", err))?;

            if proof_outputs.anchor_root != anchor_root_array {
                return Err(errors::bad_request_400(
                    "Proof outputs mismatch",
                    "anchor_root does not match",
                ));
            }
            if proof_outputs.nullifier != nullifier_array {
                return Err(errors::bad_request_400(
                    "Proof outputs mismatch",
                    "nullifier does not match",
                ));
            }
            // For transfers, withdraw_amount must be 0
            if proof_outputs.withdraw_amount != 0 {
                return Err(errors::bad_request_400(
                    "Proof outputs mismatch",
                    "withdraw_amount must be 0 for transfers",
                ));
            }

            midnight_privacy::cache_pre_verified_spend(proof_outputs.clone());
            WorkerTxIntent::Transfer { proof_outputs }
        } else if transaction_data
            .get("deposit")
            .and_then(|v| v.as_object())
            .is_some()
        {
            let proof_outputs_str = model.proof_outputs.trim();
            if !(proof_outputs_str.is_empty() || proof_outputs_str == "{}") {
                return Err(errors::bad_request_400(
                    "Unexpected proof outputs",
                    "Deposit transactions should not include proof outputs",
                ));
            }
            WorkerTxIntent::Deposit
        } else {
            return Err(errors::bad_request_400(
                "Unsupported transaction",
                "Only deposit, transfer or withdraw transactions can use this endpoint",
            ));
        };

        // Use the OPTIMIZED path - decode serialized transaction directly
        let result = match worker_tx_intent {
            WorkerTxIntent::Withdraw { proof_outputs } => {
                let proof_outputs_clone = proof_outputs.clone();
                crate::common::cache_pre_verified_midnight_transaction(tx_hash_value, proof_outputs_clone);
                let sequencer = state.sequencer.clone();
                crate::common::with_pre_verified_midnight_transaction(proof_outputs, async move {
                    sequencer
                        .accept_serialized_pre_authenticated_tx(
                            serialized_tx_base64.clone(),
                            tx_hash_value,
                        )
                        .await
                })
                .await
            }
            WorkerTxIntent::Transfer { proof_outputs } => {
                let proof_outputs_clone = proof_outputs.clone();
                crate::common::cache_pre_verified_midnight_transaction(tx_hash_value, proof_outputs_clone);
                let sequencer = state.sequencer.clone();
                crate::common::with_pre_verified_midnight_transaction(proof_outputs, async move {
                    sequencer
                        .accept_serialized_pre_authenticated_tx(
                            serialized_tx_base64.clone(),
                            tx_hash_value,
                        )
                        .await
                })
                .await
            }
            WorkerTxIntent::Deposit => {
                let sequencer = state.sequencer.clone();
                sequencer
                    .accept_serialized_pre_authenticated_tx(
                        serialized_tx_base64.clone(),
                        tx_hash_value,
                    )
                    .await
            }
        };

        let tx_with_hash = match result {
            Ok(res) => res,
            Err(e) => {
                crate::common::clear_tx_pre_authenticated(&tx_hash_value);
                crate::common::remove_pre_verified_midnight_transaction(&tx_hash_value);
                // Do not clear the pre-verified spend here; allow STF to consume it.
                if e.status.is_server_error() {
                    tracing::error!(error = ?e, "Error accepting worker transaction");
                }
                return Err(IntoResponse::into_response(e));
            }
        };

        let tx_hash_value = tx_with_hash.tx_hash.clone();
        let confirmation = tx_with_hash.confirmation;
        let sequencer_metrics = take_sequencer_metrics(&tx_hash_value);
        let response_payload = TxInfoWithConfirmation {
            id: tx_hash_value.clone(),
            confirmation,
            status: TxStatus::Submitted,
            sequencer_metrics,
        };

        let serialized_response = serde_json::to_string(&response_payload).map_err(|err| {
            errors::internal_server_error_response_500(format!(
                "Failed to serialize sequencer response for worker tx {tx_hash_value}: {err}"
            ))
        })?;

        let mut active_model: worker_verified_transactions::ActiveModel = model.into();
        active_model.transaction_state =
            Set(worker_verified_transactions::TransactionState::Accepted);
        active_model.sequencer_status = Set(Some(serialized_response));

        if let Err(err) = active_model.update(db).await {
            crate::common::clear_tx_pre_authenticated(&tx_hash_value);
            crate::common::remove_pre_verified_midnight_transaction(&tx_hash_value);
            // Do not clear the pre-verified spend here; allow STF to consume it.
            return Err(errors::database_error_response_500(err));
        }

        crate::common::clear_tx_pre_authenticated(&tx_hash_value);
        crate::common::remove_pre_verified_midnight_transaction(&tx_hash_value);
        // Do not clear the pre-verified spend here; allow STF to consume it.

        Ok(response_payload.into())
    }

    async fn axum_accept_tx(
        state: State<Self>,
        tx: Json<AcceptTx>,
    ) -> ApiResult<
        TxInfoWithConfirmation<DaBlobHash<<Seq::Da as DaService>::Spec>, Seq::Confirmation>,
    > {
        let raw_tx = RawTx::new(tx.0.body.blob);
        let baked_tx = <<Seq::Rt as Runtime<Seq::Spec>>::Auth as TransactionAuthenticator<
            Seq::Spec,
        >>::encode_with_standard_auth(raw_tx);

        let tx_with_hash = tokio::spawn(async move { state.sequencer.accept_tx(baked_tx).await })
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "A panic occurred while accepting a transaction");
                sov_rest_utils::errors::internal_server_error_response_500(
                    "An internal error occurred while processing the transaction",
                )
            })?
            .map_err(|e| {
                if e.status.is_server_error() {
                    tracing::error!(error = ?e, "Error accepting transaction");
                }
                IntoResponse::into_response(e)
            })?;

        Ok(TxInfoWithConfirmation {
            id: tx_with_hash.tx_hash,
            confirmation: tx_with_hash.confirmation,
            status: TxStatus::Submitted,
            sequencer_metrics: None,
        }
        .into())
    }

    async fn subscribe_to_events(
        State(state): State<Self>,
        filter: FilterQuery,
        ws: WebSocketUpgrade,
    ) -> impl IntoResponse {
        use futures::future;
        ws.on_upgrade(|socket| async move {
            let stream = state
                .sequencer
                .subscribe_events()
                .await
                .unwrap_or_else(|| futures::stream::empty().boxed())
                .filter(|event| match (event, &filter.filter) {
                    // Only filter events if the event is Ok (don't drop the errors!) and there is a filter configured.
                    (Ok(event), Some(filter)) => future::ready(filter.matches(&event.key)),
                    (_, _) => future::ready(true),
                });
            serve_generic_ws_subscription(socket, stream, state.shutdown_receiver.clone()).await;
        })
    }

    async fn subscribe_to_transactions(
        State(state): State<Self>,
        start_from: Option<Query<StartFrom>>,
        ws: WebSocketUpgrade,
    ) -> impl IntoResponse {
        let start_from = start_from.map(|start_from| start_from.0.start_from);
        ws.on_upgrade(move |socket| async move {
            let stream =
                Self::subscribe_txs_starting_from(start_from, state.sequencer.clone()).await;
            serve_generic_ws_subscription(socket, stream, state.shutdown_receiver.clone()).await;
        })
    }

    async fn subscribe_txs_starting_from(
        start_from: Option<u64>,
        sequencer: Arc<Seq>,
    ) -> Pin<Box<dyn futures::Stream<Item = anyhow::Result<ApiAcceptedTx<Seq::Confirmation>>> + Send>>
    {
        let Some(stream) = sequencer.subscribe_transactions(start_from).await else {
            return futures::stream::empty().boxed();
        };
        match stream {
            Ok(stream) => stream,
            Err(e) => futures::stream::once(futures::future::ready(Err(e))).boxed(),
        }
    }

    #[cfg(feature = "test-utils")]
    async fn subscribe_to_blobs_from_blob_sender(
        State(state): State<Self>,
        ws: WebSocketUpgrade,
    ) -> impl IntoResponse {
        ws.on_upgrade(|socket| async move {
            let stream = state
                .sequencer
                .subscribe_blobs_from_blob_sender()
                .await
                .map(|receiver| {
                    BroadcastStream::new(receiver)
                        .map_err(|err| anyhow::anyhow!("Error creating broadcast stream: {err}"))
                        .boxed()
                })
                .unwrap_or_else(|| futures::stream::empty().boxed());
            serve_generic_ws_subscription(socket, stream, state.shutdown_receiver.clone()).await;
        })
    }

    #[cfg(feature = "test-utils")]
    async fn axum_force_close_batch(state: State<Self>) -> ApiResult<()> {
        state
            .sequencer
            .force_close_current_batch()
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Error force closing batch");
                errors::internal_server_error_response_500("Unable to force close batch")
                    .into_response()
            })?;

        Ok(().into())
    }

    /// Subscribe to state updates. Note that notifications may be delivered out of order.
    #[cfg(feature = "test-utils")]
    async fn subscribe_to_state_updates_unstable(
        State(state): State<Self>,
        ws: WebSocketUpgrade,
    ) -> impl IntoResponse {
        ws.on_upgrade(|socket| async move {
            let stream = state
                .sequencer
                .subscribe_state_updates_unstable()
                .await
                .map(|receiver| {
                    BroadcastStream::new(receiver)
                        .map_err(|err| anyhow::anyhow!("Error creating broadcast stream: {err}"))
                        .boxed()
                })
                .unwrap_or_else(|| futures::stream::empty().boxed());
            serve_generic_ws_subscription(socket, stream, state.shutdown_receiver.clone()).await;
        })
    }

    async fn axum_get_event(
        state: State<Self>,
        Path(event_number): Path<u64>,
    ) -> ApiResult<RuntimeEventResponse<<Seq::Rt as RuntimeEventProcessor>::RuntimeEvent>> {
        let next_event_number = event_number.checked_add(1).ok_or(errors::bad_request_400(
            "u64::MAX is not a valid event number",
            "",
        ))?;
        let mut events = state
            .sequencer
            .list_events(event_number..next_event_number)
            .await
            .map_err(|_| errors::database_error_500("Unable to retrieve event").into_response())?;
        if let Some(event) = events.pop() {
            Ok(event.into())
        } else {
            Err(errors::not_found_404("Event", event_number))
        }
    }

    async fn axum_list_events(
        state: State<Self>,
        pagination_opt: Option<Query<Pagination<String>>>,
    ) -> ApiResult<
        PaginatedResponse<
            RuntimeEventResponse<<Seq::Rt as RuntimeEventProcessor>::RuntimeEvent>,
            String,
        >,
    > {
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
        // Note: The previous version of this code returned one more than the requested number of events.
        // This is now fixed.
        let end = start
            .checked_add(pagination.size as u64)
            .unwrap_or(u64::MAX);

        let events =
            state.sequencer.list_events(start..end).await.map_err(|_| {
                errors::database_error_500("Unable to retrieve events").into_response()
            })?;
        let next_cursor = start + events.len() as u64;
        let response = PaginatedResponse {
            items: events,
            next_cursor: Some(next_cursor.to_string()),
        };
        Ok(response.into())
    }
}

#[serde_as]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
#[allow(missing_docs)]
pub struct Base64Blob {
    #[serde_as(as = "Base64")]
    pub blob: Vec<u8>,
}

/// The input of the axum accept_tx endpoint.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AcceptTx {
    #[allow(missing_docs)]
    pub body: Base64Blob,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct TxInfo<DaTransactionId> {
    id: TxHash,
    #[serde(flatten)]
    status: TxStatus<DaTransactionId>,
}

/// The output of the axum accept_tx endpoint.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[allow(missing_docs)]
pub struct TxInfoWithConfirmation<DaTransactionId, Confirmation> {
    pub id: TxHash,
    #[serde(flatten)]
    pub confirmation: Confirmation,
    #[serde(flatten)]
    pub status: TxStatus<DaTransactionId>,
    /// Optional sequencer timing metrics for this transaction (when available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequencer_metrics: Option<SequencerMetrics>,
}

/// An accepted transaction, with the transaction body and confirmation data.
#[serde_with::serde_as]
#[derive(Clone, serde::Serialize)]
pub struct ApiAcceptedTx<Confirmation> {
    /// The hex encoded transaction hash
    pub id: TxHash,
    /// The base64 encoded transaction body
    #[serde_as(as = "serde_with::base64::Base64")]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tx: Vec<u8>,
    /// The confirmation data
    #[serde(flatten)]
    pub confirmation: Confirmation,
}

impl<C> From<AcceptedTx<C>> for ApiAcceptedTx<C> {
    fn from(tx: AcceptedTx<C>) -> Self {
        Self {
            id: tx.tx_hash,
            tx: tx.tx.data,
            confirmation: tx.confirmation,
        }
    }
}

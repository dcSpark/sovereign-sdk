#![allow(dead_code)]
use crate::preferred::cache_warm_up_executor::StartBlockNotification;
use crate::preferred::PreferredSequencerConfig;
use crate::preferred::RollupBlockExecutor;
use crate::preferred::RollupBlockExecutorConfig;
use crate::SequencerConfig;
use crate::TxHash;
use sov_metrics::Metric;
use sov_modules_api::Spec;
use sov_modules_api::StateUpdateInfo;
use sov_modules_api::TxChangeSet;
use sov_modules_api::TransactionReceipt;
use sov_modules_api::{
    ApiTxEffect, FullyBakedTx, Runtime, RuntimeEventProcessor, TxReceiptContents,
};

use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::task::JoinHandle;

/// Global counter to track how many workers are actively processing transactions.
/// This helps prove parallelization by showing multiple workers active simultaneously.
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);

// Channel size for parallel transaction processing.
// This should be large enough to accommodate multiple transactions being processed simultaneously
// by different workers, but not so large that it causes memory issues.
const PARALLEL_TX_CHANNEL_SIZE: usize = 2048;

/// Result of parallel transaction execution that will be sent back to the main sequencer.
/// Contains all the information needed to finalize the transaction without re-executing it.
pub struct ParallelizedResponse<S: Spec> {
    /// The original transaction hash for identification
    pub tx_hash: TxHash,
    /// Raw receipt produced by the parallel worker
    pub receipt: TransactionReceipt<S>,
    /// The state changes produced by this transaction
    pub tx_changes: TxChangeSet,
    /// Remaining slot gas after execution
    pub remaining_slot_gas: <S as Spec>::Gas,
    /// Execution time in microseconds
    pub execution_time_micros: u64,
    /// Original transaction queue ID for ordering
    pub original_tx_queue_id: u64,
    /// Precomputed user-facing effect (saves `.into()` on the main thread)
    pub api_effect: ApiTxEffect<TxReceiptContents<S>>,
}

/// A transaction to be processed in parallel along with metadata needed for the response.
struct ParallelTxRequest<S: Spec, Rt: Runtime<S>> {
    /// The transaction to process
    tx: FullyBakedTx,
    /// Transaction hash
    tx_hash: TxHash,
    /// Original queue ID for maintaining order
    original_tx_queue_id: u64,
    /// Metadata for completion message
    sequence_number: sov_blob_storage::SequenceNumber,
    tx_len: usize,
    /// Channel to send completion message directly to the message loop
    message_sender: tokio::sync::mpsc::Sender<crate::preferred::inner::Message<S, Rt>>,
}

struct TxReceiver<S: Spec, Rt: Runtime<S>> {
    size: Arc<AtomicU64>,
    receiver: flume::Receiver<ParallelTxRequest<S, Rt>>,
}

impl<S: Spec, Rt: Runtime<S>> Clone for TxReceiver<S, Rt> {
    fn clone(&self) -> Self {
        Self {
            size: self.size.clone(),
            receiver: self.receiver.clone(),
        }
    }
}

/// Parallel transaction executor for midnight privacy module transactions.
/// 
/// This executor processes transactions in parallel using a pool of workers,
/// each with its own executor instance. Transactions are fully processed
/// (except for the final batch addition) and results are sent back to the
/// main sequencer for ordering and finalization.
pub(crate) struct ParallelTxExecutor<S: Spec, Rt: Runtime<S>> {
    start_block_notification_sender: tokio::sync::watch::Sender<Option<StartBlockNotification<S>>>,
    tx_sender: flume::Sender<ParallelTxRequest<S, Rt>>,
    size: Arc<AtomicU64>,
    _phantom: std::marker::PhantomData<Rt>,
}

impl<S: Spec, Rt: Runtime<S>> Clone for ParallelTxExecutor<S, Rt> {
    fn clone(&self) -> Self {
        Self {
            start_block_notification_sender: self.start_block_notification_sender.clone(),
            tx_sender: self.tx_sender.clone(),
            size: self.size.clone(),
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<S: Spec, Rt: Runtime<S>> ParallelTxExecutor<S, Rt> {
    /// Send a batch start notification to all workers.
    /// This updates the state checkpoint for all parallel executors.
    pub(crate) fn send_batch_start_notification(&self, data: StartBlockNotification<S>) {
        // This `send` does not block.
        let _ = self.start_block_notification_sender.send(Some(data));
    }

    /// Send a transaction for parallel processing.
    /// 
    /// Worker will send the result directly to the message channel when done.
    /// Returns true if the transaction was accepted, false if the queue is full or disconnected.
    pub(crate) fn send_tx(
        &self,
        tx: FullyBakedTx,
        tx_hash: TxHash,
        original_tx_queue_id: u64,
        sequence_number: sov_blob_storage::SequenceNumber,
        tx_len: usize,
        message_sender: tokio::sync::mpsc::Sender<crate::preferred::inner::Message<S, Rt>>,
    ) -> bool
    where
        Rt: RuntimeEventProcessor + 'static,
    {
        // Update size before sending to ensure workers see correct channel size
        let size = self.size.fetch_add(1, Ordering::Relaxed);

        let request = ParallelTxRequest {
            tx,
            tx_hash,
            original_tx_queue_id,
            sequence_number,
            tx_len,
            message_sender,
        };

        // Try to send without blocking
        let res = self.tx_sender.try_send(request);

        match res {
            Ok(_) => {
                sov_metrics::track_metrics(|t| {
                    t.submit(ParallelTxExecutorMetrics {
                        tx_channel_size: size + 1,
                        worker_count: 0,
                    });
                });

                tracing::debug!(
                    tx_hash = %tx_hash,
                    queue_id = original_tx_queue_id,
                    channel_size = size + 1,
                    "Transaction sent to parallel executor"
                );

                true
            }
            Err(flume::TrySendError::Full(_)) => {
                let _size = self.size.fetch_sub(1, Ordering::Relaxed);
                tracing::warn!(
                    tx_hash = %tx_hash,
                    "Parallel tx queue is full. Transaction will be processed sequentially. \
                     Consider increasing num_parallel_tx_workers."
                );
                false
            }
            Err(flume::TrySendError::Disconnected(_)) => {
                self.size.fetch_sub(1, Ordering::Relaxed);
                tracing::error!("Parallel tx executor channel disconnected");
                false
            }
        }
    }

    /// Spawn the parallel execution task with a pool of workers.
    /// 
    /// Creates N worker threads (based on config) that will process transactions
    /// in parallel. Each worker maintains its own executor instance with a cloned
    /// state checkpoint.
    pub(crate) async fn spawn_execution_task(
        info: StateUpdateInfo<S::Storage>,
        exec_config: RollupBlockExecutorConfig<S>,
        seq_config: SequencerConfig<S::Address, PreferredSequencerConfig>,
    ) -> (Self, Vec<JoinHandle<()>>)
    where
        Rt: 'static,
    {
        let (tx_sender, tx_receiver) = flume::bounded(PARALLEL_TX_CHANNEL_SIZE);
        let size = Arc::new(AtomicU64::new(0));
        let tx_receiver = TxReceiver {
            size: size.clone(),
            receiver: tx_receiver,
        };

        let (start_block_notification_sender, start_block_notification_receiver) =
            tokio::sync::watch::channel(None);

        // Determine the number of workers with priority order:
        // 1. Environment variable SOV_PARALLEL_TX_WORKERS (if set)
        // 2. Config value num_parallel_tx_workers (if set and non-zero)
        // 3. Default to number of CPU cores
        let num_workers = match std::env::var("SOV_PARALLEL_TX_WORKERS") {
            Ok(env_value) => {
                match env_value.parse::<usize>() {
                    Ok(n) => {
                        tracing::info!(
                            workers = n,
                            "Using SOV_PARALLEL_TX_WORKERS from environment variable"
                        );
                        n
                    }
                    Err(_) => {
                        tracing::warn!(
                            value = %env_value,
                            "Invalid SOV_PARALLEL_TX_WORKERS value, falling back to config or CPU cores"
                        );
                        // Fall through to config-based logic
                        let configured_workers = seq_config
                            .sequencer_kind_config
                            .num_parallel_tx_workers
                            .unwrap_or(0);
                        
                        if configured_workers == 0 {
                            std::thread::available_parallelism()
                                .map(|n| n.get())
                                .unwrap_or(1)
                        } else {
                            configured_workers
                        }
                    }
                }
            }
            Err(_) => {
                // No environment variable set, use config value
                let configured_workers = seq_config
                    .sequencer_kind_config
                    .num_parallel_tx_workers
                    .unwrap_or(0);
                
                if configured_workers == 0 {
                    // Default to number of available CPU cores
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(1)
                } else {
                    configured_workers
                }
            }
        };

        // Ensure at least 1 worker to prevent hangs when transactions are enqueued
        let num_workers = num_workers.max(1);

        let config_source = if std::env::var("SOV_PARALLEL_TX_WORKERS").is_ok() {
            "environment"
        } else {
            "config"
        };
        
        tracing::info!(
            num_workers,
            source = config_source,
            channel_size = PARALLEL_TX_CHANNEL_SIZE,
            "Starting parallel transaction executor worker pool"
        );

        let mut handles = Vec::new();
        for worker_id in 0..num_workers {
            let worker = Self::spawn_worker(
                worker_id,
                info.clone(),
                exec_config.clone(),
                seq_config.clone(),
                tx_receiver.clone(),
                start_block_notification_receiver.clone(),
            );

            handles.push(worker);
        }

        (
            Self {
                tx_sender,
                start_block_notification_sender,
                size,
                _phantom: std::marker::PhantomData,
            },
            handles,
        )
    }

    /// Spawn a single worker thread that processes transactions in parallel.
    fn spawn_worker(
        worker_id: usize,
        info: StateUpdateInfo<S::Storage>,
        exec_config: RollupBlockExecutorConfig<S>,
        seq_config: SequencerConfig<S::Address, PreferredSequencerConfig>,
        tx_receiver: TxReceiver<S, Rt>,
        mut start_block_notification_receiver: tokio::sync::watch::Receiver<
            Option<StartBlockNotification<S>>,
        >,
    ) -> JoinHandle<()>
    where
        Rt: 'static,
    {
        tokio::spawn(async move {
            tracing::debug!(worker_id, "Parallel tx worker starting");
            
            let mut shutdown_receiver = exec_config.shutdown_receiver.clone();
            let mut executor = RollupBlockExecutor::<_, Rt>::new(
                &info,
                exec_config,
                seq_config.clone(),
                Default::default(),
            );

            let mut is_started = false;
            let mut txs_processed: u64 = 0;

            loop {
                tokio::select! {
                    _ = start_block_notification_receiver.changed() => {
                        let notify = start_block_notification_receiver.borrow().clone();
                        if let Some(notify) = notify {
                            tracing::debug!(
                                worker_id,
                                txs_processed,
                                "Parallel worker received batch start notification"
                            );

                            // Shutdown the old executor and start fresh with new state
                            let _ = executor.shutdown().await;
                            Self::start_block(notify, &mut executor).await;
                            is_started = true;
                            txs_processed = 0;
                        }
                    }

                    request = tx_receiver.receiver.recv_async() => {
                        let request = match request {
                            Ok(req) => {
                                tx_receiver.size.fetch_sub(1, Ordering::Relaxed);
                                req
                            },
                            Err(flume::RecvError::Disconnected) => {
                                tracing::info!(worker_id, txs_processed, "Parallel worker shutting down (channel disconnected)");
                                return;
                            },
                        };

                        if !is_started {
                            tracing::debug!(
                                worker_id,
                                tx_hash = %request.tx_hash,
                                "Parallel worker received tx before batch start; waiting for start notification"
                            );

                            // Wait until the batch start notification arrives (or shutdown)
                            loop {
                                tokio::select! {
                                    _ = start_block_notification_receiver.changed() => {
                                        let notify_opt = start_block_notification_receiver.borrow().clone();
                                        if let Some(notify) = notify_opt {
                                            // Restart executor with new state and mark as started
                                            let _ = executor.shutdown().await;
                                            Self::start_block(notify, &mut executor).await;
                                            is_started = true;
                                            txs_processed = 0;
                                            break;
                                        }
                                    }
                                    _ = shutdown_receiver.changed() => {
                                        tracing::info!(worker_id, "Parallel worker shutting down while waiting for start");
                                        return;
                                    }
                                }
                            }
                        }

                        let start_time = std::time::Instant::now();
                        let start_timestamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_micros();

                        // Increment active workers counter to track concurrency
                        let active_count = ACTIVE_WORKERS.fetch_add(1, Ordering::SeqCst) + 1;

                        tracing::debug!(
                            worker_id,
                            tx_hash = %request.tx_hash,
                            start_timestamp_micros = start_timestamp,
                            active_workers = active_count,
                            "[PARALLEL] Worker starting transaction execution"
                        );

                        tracing::trace!(
                            worker_id,
                            tx_hash = %request.tx_hash,
                            queue_id = request.original_tx_queue_id,
                            start_timestamp,
                            active_workers = active_count,
                            "Processing transaction in parallel"
                        );

                        // Process the transaction using our executor
                        use crate::preferred::cache_warm_up_executor::FullyBakedTxWithMaybeChangeSet;
                        let baked_tx = FullyBakedTxWithMaybeChangeSet::new(request.tx);
                        let result = executor.execute_tx_return_receipt(baked_tx).await;

                        // Decrement active workers counter
                        let active_count_after = ACTIVE_WORKERS.fetch_sub(1, Ordering::SeqCst) - 1;

                        match result {
                            Ok((receipt, tx_changes, remaining_slot_gas, execution_time_micros)) => {
                                let elapsed = start_time.elapsed();
                                let end_timestamp = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap()
                                    .as_micros();
                                txs_processed += 1;

                                tracing::debug!(
                                    worker_id,
                                    tx_hash = %request.tx_hash,
                                    end_timestamp_micros = end_timestamp,
                                    elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                                    elapsed_micros = elapsed.as_micros(),
                                    active_workers = active_count_after,
                                    "[PARALLEL] Worker finished transaction execution"
                                );

                                tracing::debug!(
                                    worker_id,
                                    tx_hash = %request.tx_hash,
                                    queue_id = request.original_tx_queue_id,
                                    elapsed_micros = elapsed.as_micros(),
                                    txs_processed,
                                    "Transaction processed successfully in parallel"
                                );

                                // Heavy-ish conversion done in the worker.
                                let api_effect: ApiTxEffect<TxReceiptContents<S>> =
                                    receipt.receipt.clone().into();

                                let parallel_response = ParallelizedResponse::<S> {
                                    tx_hash: request.tx_hash,
                                    receipt,
                                    tx_changes,
                                    remaining_slot_gas,
                                    execution_time_micros,
                                    original_tx_queue_id: request.original_tx_queue_id,
                                    api_effect,
                                };

                                // Send completion message directly to the message loop
                                let msg = crate::preferred::inner::Message::ParallelTxCompleted {
                                    parallel_response,
                                    sequence_number: request.sequence_number,
                                    tx_len: request.tx_len,
                                    reason: "parallel_tx_completed",
                                };
                                
                                // It's safe to ignore errors if the channel is closed (shutdown)
                                if let Err(err) = request.message_sender.send(msg).await {
                                    tracing::debug!(
                                        worker_id,
                                        tx_hash = %request.tx_hash,
                                        "Failed to send completion message (likely shutdown): {:?}",
                                        err
                                    );
                                }
                            }
            Err(err) => {
                tracing::debug!(
                    worker_id,
                    tx_hash = %request.tx_hash,
                    %err,
                    "Parallel worker failed to execute transaction"
                );
                // Notify the main sequencer so it can clean up the HTTP waiter
                // and decrement the in-flight parallel counter, instead of
                // leaving the request hanging indefinitely.
                let fail_msg = crate::preferred::inner::Message::ParallelTxFailed {
                    tx_hash: request.tx_hash,
                    reason: "parallel_tx_failed",
                };
                if let Err(send_err) = request.message_sender.send(fail_msg).await {
                    tracing::debug!(
                        worker_id,
                        tx_hash = %request.tx_hash,
                        "Failed to send ParallelTxFailed message (likely shutdown): {:?}",
                        send_err
                    );
                }
                continue;
            }
                        }
                    }

                    _ = shutdown_receiver.changed() => {
                        tracing::info!(worker_id, txs_processed, "Parallel worker shutting down");
                        return;
                    }
                }
            }
        })
    }

    async fn start_block(
        notify: StartBlockNotification<S>,
        executor: &mut RollupBlockExecutor<S, Rt>,
    ) {
        executor
            .start_rollup_block_with_provided_state_roots(
                notify.data,
                notify.checkpoint,
                notify.state_roots,
            )
            .await;
    }
}

#[derive(Debug)]
pub(crate) struct ParallelTxExecutorMetrics {
    tx_channel_size: u64,
    worker_count: u64,
}

impl Metric for ParallelTxExecutorMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_sequencer_parallel_tx_metrics"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{} tx_channel_size={},worker_count={}",
            self.measurement_name(),
            self.tx_channel_size,
            self.worker_count,
        )
    }
}

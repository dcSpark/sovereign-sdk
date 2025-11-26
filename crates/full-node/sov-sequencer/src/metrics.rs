use std::io::Write;

use sov_metrics::Metric;

pub fn track_sequence_number(sequence_number: u64) {
    sov_metrics::track_metrics(|tracker| {
        tracker.submit_inline(
            "sov_rollup_current_sequence_number",
            format!("current_sequence_number={sequence_number}"),
        );
    });
}

pub fn track_in_progress_batch_size(num_txs: u64) {
    sov_metrics::track_metrics(|tracker| {
        tracker.submit_inline(
            "sov_rollup_in_progress_batch_size",
            format!("num_txs={num_txs}"),
        );
    });
}

#[derive(Debug)]
pub struct PreferredSequencerUpdateStateMetrics {
    pub duration: std::time::Duration,
    pub total_message_processing_duration: std::time::Duration,
    pub batches_count: u64,
    pub transactions_count: u64,
    pub in_progress_batch: bool,
    pub time_spent_fetching_batches: std::time::Duration,
}

impl Metric for PreferredSequencerUpdateStateMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_preferred_sequencer_update_state"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{} duration_ms={},total_message_processing_duration_ms={},fetch_batches_duration_us={},batches_count={},transactions_count={},in_progress_batch={}",
            self.measurement_name(),
            self.duration.as_millis(),
            self.total_message_processing_duration.as_millis(),
            self.time_spent_fetching_batches.as_micros(),
            self.batches_count,
            self.transactions_count,
            self.in_progress_batch
        )
    }
}

#[derive(Debug)]
pub struct PreferredSequencerChannelMetrics {
    pub duration: std::time::Duration,
    pub reason: &'static str,
    pub channel_size: u32,
}

impl Metric for PreferredSequencerChannelMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_preferred_sequencer_channel"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{},reason={} duration_us={},channel_size={}",
            self.measurement_name(),
            self.reason,
            self.duration.as_micros(),
            self.channel_size,
        )
    }
}

#[derive(Debug)]
pub struct PreferredSequencerChannelMetricsBatch {
    pub metrics: Vec<PreferredSequencerChannelMetrics>,
}

impl Metric for PreferredSequencerChannelMetricsBatch {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_preferred_sequencer_channel"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        if self.metrics.is_empty() {
            return Ok(());
        }
        for (i, metric) in self.metrics.iter().enumerate() {
            metric.serialize_for_telegraf(buffer)?;
            if i != (self.metrics.len() - 1) {
                buffer.push(b'\n');
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct PreferredSequencerExecutorEventMetrics {
    pub event_type: &'static str,
    pub duration: std::time::Duration,
    pub batch_size: usize,
}

impl Metric for PreferredSequencerExecutorEventMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_preferred_sequencer_executor_event"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{},event_type={} duration_us={},batch_size={}",
            self.measurement_name(),
            self.event_type,
            self.duration.as_micros(),
            self.batch_size,
        )
    }
}

#[derive(Debug)]
pub struct PreferredBatchClosingMetrics {
    pub reason: &'static str,
    pub num_txs: u64,
    pub batch_size_bytes: u64,
    pub batch_open_duration_us: u64,
    pub batch_execution_time_micros: u64,
    pub batch_execution_time_limit_us: u64,
    pub batch_execution_time_headroom_us: u64,
    pub pending_parallel_count: u32,
    pub configured_max_batch_size_bytes: u64,
}

impl Metric for PreferredBatchClosingMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_preferred_batch_closing"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{},reason={} num_txs={},batch_size_bytes={},batch_open_duration_us={},batch_execution_time_us={},batch_execution_time_limit_us={},batch_execution_time_headroom_us={},pending_parallel_count={},configured_max_batch_size_bytes={}",
            self.measurement_name(),
            self.reason,
            self.num_txs,
            self.batch_size_bytes,
            self.batch_open_duration_us,
            self.batch_execution_time_micros,
            self.batch_execution_time_limit_us,
            self.batch_execution_time_headroom_us,
            self.pending_parallel_count,
            self.configured_max_batch_size_bytes
        )
    }
}

#[derive(Debug)]
pub struct PreferredSequencerFetchBatchesToReplayMetrics {
    pub duration: std::time::Duration,
    pub num_batches: u64,
    pub num_transactions: usize,
}

impl Metric for PreferredSequencerFetchBatchesToReplayMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_preferred_sequencer_fetch_batches_to_replay"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{} duration_us={},num_batches={},num_transactions={}",
            self.measurement_name(),
            self.duration.as_micros(),
            self.num_batches,
            self.num_transactions,
        )
    }
}

#[derive(Debug)]
pub struct PreferredSequencerPruneMetrics {
    pub duration_ms: u64,
}

impl Metric for PreferredSequencerPruneMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_preferred_sequencer_prune"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{} duration_ms={}",
            self.measurement_name(),
            self.duration_ms,
        )
    }
}

#[derive(Debug, Default)]
pub struct PreferredSequencerExecutorEventSendingMetrics {
    pub blocked_for_us: u64,
    pub queue_depth: usize,
}

impl Metric for PreferredSequencerExecutorEventSendingMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_preferred_sequencer_executor_event_sending"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{} blocked_for_us={},queue_depth={}",
            self.measurement_name(),
            self.blocked_for_us,
            self.queue_depth,
        )
    }
}

#[derive(Debug)]
pub struct ParallelTxFastPathMetrics {
    pub total_duration_us: u64,
    pub apply_changes_us: u64,
    pub build_accepted_us: u64,
    pub cache_us: u64,
}

impl Metric for ParallelTxFastPathMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_parallel_tx_fast_path"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{} total_duration_us={},apply_changes_us={},build_accepted_us={},cache_us={}",
            self.measurement_name(),
            self.total_duration_us,
            self.apply_changes_us,
            self.build_accepted_us,
            self.cache_us,
        )
    }
}

#[derive(Debug)]
pub struct MessageLoopThroughputMetrics {
    pub messages_processed: u64,
    pub duration_ms: u64,
    pub avg_channel_size: u64,
    pub max_channel_size: u32,
}

impl Metric for MessageLoopThroughputMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_message_loop_throughput"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{} messages_processed={},duration_ms={},avg_channel_size={},max_channel_size={}",
            self.measurement_name(),
            self.messages_processed,
            self.duration_ms,
            self.avg_channel_size,
            self.max_channel_size,
        )
    }
}

#[derive(Debug)]
pub struct ParallelExecutorUtilizationMetrics {
    pub active_workers: u32,
    pub total_workers: u32,
    pub pending_tx_count: u32,
    pub tx_channel_size: usize,
}

impl Metric for ParallelExecutorUtilizationMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_parallel_executor_utilization"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{} active_workers={},total_workers={},pending_tx_count={},tx_channel_size={}",
            self.measurement_name(),
            self.active_workers,
            self.total_workers,
            self.pending_tx_count,
            self.tx_channel_size,
        )
    }
}

#[derive(Debug)]
pub struct HttpWaitTimeMetrics {
    pub total_wait_us: u64,
    pub queue_wait_us: u64,
    pub execution_wait_us: u64,
    pub parallel_tx: bool,
}

impl Metric for HttpWaitTimeMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_http_wait_time"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{},parallel_tx={} total_wait_us={},queue_wait_us={},execution_wait_us={}",
            self.measurement_name(),
            self.parallel_tx,
            self.total_wait_us,
            self.queue_wait_us,
            self.execution_wait_us,
        )
    }
}

#[derive(Debug)]
pub struct BatchClosingMetrics {
    pub reason: &'static str,
    pub batch_size_bytes: u64,
    pub num_txs: u64,
    pub gas_remaining: u64,
    pub batch_open_duration_us: u64,
    pub pending_parallel_count: u32,
}

impl Metric for BatchClosingMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_batch_closing"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{},reason={} batch_size_bytes={},num_txs={},gas_remaining={},batch_open_duration_us={},pending_parallel_count={}",
            self.measurement_name(),
            self.reason,
            self.batch_size_bytes,
            self.num_txs,
            self.gas_remaining,
            self.batch_open_duration_us,
            self.pending_parallel_count,
        )
    }
}

#[derive(Debug)]
pub struct TransactionCacheContentionMetrics {
    pub operation: &'static str,
    pub lock_wait_us: u64,
    pub operation_duration_us: u64,
}

impl Metric for TransactionCacheContentionMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_tx_cache_contention"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{},operation={} lock_wait_us={},operation_duration_us={}",
            self.measurement_name(),
            self.operation,
            self.lock_wait_us,
            self.operation_duration_us,
        )
    }
}

#[derive(Debug)]
pub struct BatchLifecycleMetrics {
    pub event: &'static str,
    pub batch_open_duration_us: u64,
    pub num_txs: u64,
    pub batch_size_bytes: u64,
    pub batch_execution_time_us: u64,
    pub pending_parallel_count: u32,
    pub message_queue_depth: u32,
}

impl Metric for BatchLifecycleMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_batch_lifecycle"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{},event={} batch_open_duration_us={},num_txs={},batch_size_bytes={},batch_execution_time_us={},pending_parallel_count={},message_queue_depth={}",
            self.measurement_name(),
            self.event,
            self.batch_open_duration_us,
            self.num_txs,
            self.batch_size_bytes,
            self.batch_execution_time_us,
            self.pending_parallel_count,
            self.message_queue_depth,
        )
    }
}

#[derive(Debug)]
pub struct TransactionThroughputMetrics {
    pub txs_accepted: u64,
    pub txs_parallel: u64,
    pub txs_sequential: u64,
    pub duration_ms: u64,
    pub avg_tx_processing_us: u64,
}

impl Metric for TransactionThroughputMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_tx_throughput"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{} txs_accepted={},txs_parallel={},txs_sequential={},duration_ms={},avg_tx_processing_us={}",
            self.measurement_name(),
            self.txs_accepted,
            self.txs_parallel,
            self.txs_sequential,
            self.duration_ms,
            self.avg_tx_processing_us,
        )
    }
}

#[derive(Debug)]
pub struct BatchThroughputMetrics {
    pub batches_closed: u64,
    pub total_txs: u64,
    pub duration_ms: u64,
    pub avg_txs_per_batch: u64,
    pub avg_batch_duration_ms: u64,
}

impl Metric for BatchThroughputMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_batch_throughput"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{} batches_closed={},total_txs={},duration_ms={},avg_txs_per_batch={},avg_batch_duration_ms={}",
            self.measurement_name(),
            self.batches_closed,
            self.total_txs,
            self.duration_ms,
            self.avg_txs_per_batch,
            self.avg_batch_duration_ms,
        )
    }
}

#[derive(Debug)]
pub struct ParallelExecutorQueueMetrics {
    pub queue_depth: usize,
    pub total_capacity: usize,
    pub utilization_percent: u64,
    pub enqueued_count: u64,
    pub rejected_count: u64,
}

impl Metric for ParallelExecutorQueueMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_parallel_queue"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{} queue_depth={},total_capacity={},utilization_percent={},enqueued_count={},rejected_count={}",
            self.measurement_name(),
            self.queue_depth,
            self.total_capacity,
            self.utilization_percent,
            self.enqueued_count,
            self.rejected_count,
        )
    }
}

#[derive(Debug)]
pub struct TransactionExecutionDetailMetrics {
    pub tx_type: &'static str,
    pub total_duration_us: u64,
    pub queue_wait_us: u64,
    pub execution_us: u64,
    pub post_process_us: u64,
}

impl Metric for TransactionExecutionDetailMetrics {
    fn measurement_name(&self) -> &'static str {
        "sov_rollup_tx_execution_detail"
    }

    fn serialize_for_telegraf(&self, buffer: &mut Vec<u8>) -> std::io::Result<()> {
        write!(
            buffer,
            "{},tx_type={} total_duration_us={},queue_wait_us={},execution_us={},post_process_us={}",
            self.measurement_name(),
            self.tx_type,
            self.total_duration_us,
            self.queue_wait_us,
            self.execution_us,
            self.post_process_us,
        )
    }
}

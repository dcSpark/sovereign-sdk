use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

const PROGRESS_LOG_STEPS: usize = 20;

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

fn progress_stride(total: usize) -> usize {
    (total / PROGRESS_LOG_STEPS).max(1)
}

fn should_log_progress(completed: usize, total: usize) -> bool {
    if total == 0 || completed == 0 {
        return false;
    }

    completed >= total || completed % progress_stride(total) == 0
}

fn progress_percent(completed: usize, total: usize) -> f64 {
    if total == 0 {
        return 100.0;
    }
    (completed as f64 / total as f64) * 100.0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum StartupStage {
    Idle = 0,
    RestoringState = 1,
    CreatingWallets = 2,
    FetchingViewerFvkBundles = 3,
    FundingWallets = 4,
    WaitingForWalletBalances = 5,
    SubmittingDeposits = 6,
    WaitingForDepositNotes = 7,
    Completed = 8,
    Failed = 9,
}

impl StartupStage {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::RestoringState,
            2 => Self::CreatingWallets,
            3 => Self::FetchingViewerFvkBundles,
            4 => Self::FundingWallets,
            5 => Self::WaitingForWalletBalances,
            6 => Self::SubmittingDeposits,
            7 => Self::WaitingForDepositNotes,
            8 => Self::Completed,
            9 => Self::Failed,
            _ => Self::Idle,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::RestoringState => "restoring_state",
            Self::CreatingWallets => "creating_wallets",
            Self::FetchingViewerFvkBundles => "fetching_viewer_fvk_bundles",
            Self::FundingWallets => "funding_wallets",
            Self::WaitingForWalletBalances => "waiting_for_wallet_balances",
            Self::SubmittingDeposits => "submitting_deposits",
            Self::WaitingForDepositNotes => "waiting_for_deposit_notes",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ScaleUpStage {
    Idle = 0,
    WaitingForSequencerReady = 1,
    CreatingWallets = 2,
    FetchingViewerFvkBundles = 3,
    FundingWallets = 4,
    WaitingForWalletBalances = 5,
    SubmittingDeposits = 6,
    WaitingForDepositNotes = 7,
    AppendingWallets = 8,
    Completed = 9,
    Failed = 10,
}

impl ScaleUpStage {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::WaitingForSequencerReady,
            2 => Self::CreatingWallets,
            3 => Self::FetchingViewerFvkBundles,
            4 => Self::FundingWallets,
            5 => Self::WaitingForWalletBalances,
            6 => Self::SubmittingDeposits,
            7 => Self::WaitingForDepositNotes,
            8 => Self::AppendingWallets,
            9 => Self::Completed,
            10 => Self::Failed,
            _ => Self::Idle,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::WaitingForSequencerReady => "waiting_for_sequencer_ready",
            Self::CreatingWallets => "creating_wallets",
            Self::FetchingViewerFvkBundles => "fetching_viewer_fvk_bundles",
            Self::FundingWallets => "funding_wallets",
            Self::WaitingForWalletBalances => "waiting_for_wallet_balances",
            Self::SubmittingDeposits => "submitting_deposits",
            Self::WaitingForDepositNotes => "waiting_for_deposit_notes",
            Self::AppendingWallets => "appending_wallets",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum StartupCounter {
    WalletsCreated,
    ViewerFvkBundlesReady,
    WalletsFunded,
    WalletsBalanceReady,
    DepositsSubmitted,
    DepositNotesIndexed,
}

impl StartupCounter {
    fn metric(self) -> &'static str {
        match self {
            Self::WalletsCreated => "wallets_created",
            Self::ViewerFvkBundlesReady => "viewer_fvk_bundles_ready",
            Self::WalletsFunded => "wallets_funded",
            Self::WalletsBalanceReady => "wallets_balance_ready",
            Self::DepositsSubmitted => "deposits_submitted",
            Self::DepositNotesIndexed => "deposit_notes_indexed",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ScaleUpCounter {
    WalletsCreated,
    ViewerFvkBundlesReady,
    WalletsFunded,
    WalletsBalanceReady,
    DepositsSubmitted,
    DepositNotesIndexed,
    WalletsAppended,
}

impl ScaleUpCounter {
    fn metric(self) -> &'static str {
        match self {
            Self::WalletsCreated => "wallets_created",
            Self::ViewerFvkBundlesReady => "viewer_fvk_bundles_ready",
            Self::WalletsFunded => "wallets_funded",
            Self::WalletsBalanceReady => "wallets_balance_ready",
            Self::DepositsSubmitted => "deposits_submitted",
            Self::DepositNotesIndexed => "deposit_notes_indexed",
            Self::WalletsAppended => "wallets_appended",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct StartupProgressResponse {
    pub in_progress: bool,
    pub stage: &'static str,
    pub total_wallets: usize,
    pub wallets_created: usize,
    pub viewer_fvk_bundles_ready: usize,
    pub wallets_funded: usize,
    pub wallets_balance_ready: usize,
    pub deposits_submitted: usize,
    pub deposit_notes_indexed: usize,
    pub elapsed_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct ScaleUpProgressResponse {
    pub in_progress: bool,
    pub stage: &'static str,
    pub start_wallets: usize,
    pub target_wallets: usize,
    pub current_wallets: usize,
    pub wallets_needed: usize,
    pub wallets_remaining: usize,
    pub current_batch_size: usize,
    pub wallets_created: usize,
    pub viewer_fvk_bundles_ready: usize,
    pub wallets_funded: usize,
    pub wallets_balance_ready: usize,
    pub deposits_submitted: usize,
    pub deposit_notes_indexed: usize,
    pub wallets_appended: usize,
    pub elapsed_ms: u64,
}

pub struct StartupProgressTracker {
    in_progress: AtomicBool,
    stage: AtomicU8,
    started_at_unix_ms: AtomicU64,
    final_elapsed_ms: AtomicU64,
    total_wallets: AtomicUsize,
    wallets_created: AtomicUsize,
    viewer_fvk_bundles_ready: AtomicUsize,
    wallets_funded: AtomicUsize,
    wallets_balance_ready: AtomicUsize,
    deposits_submitted: AtomicUsize,
    notes_indexed: AtomicUsize,
}

impl StartupProgressTracker {
    pub fn new(initial_total_wallets: usize) -> Self {
        Self {
            in_progress: AtomicBool::new(false),
            stage: AtomicU8::new(StartupStage::Idle as u8),
            started_at_unix_ms: AtomicU64::new(0),
            final_elapsed_ms: AtomicU64::new(0),
            total_wallets: AtomicUsize::new(initial_total_wallets),
            wallets_created: AtomicUsize::new(0),
            viewer_fvk_bundles_ready: AtomicUsize::new(0),
            wallets_funded: AtomicUsize::new(0),
            wallets_balance_ready: AtomicUsize::new(0),
            deposits_submitted: AtomicUsize::new(0),
            notes_indexed: AtomicUsize::new(0),
        }
    }

    pub fn is_in_progress(&self) -> bool {
        self.in_progress.load(Ordering::Relaxed)
    }

    pub fn total_wallets(&self) -> usize {
        self.total_wallets.load(Ordering::Relaxed)
    }

    fn elapsed_ms(&self) -> u64 {
        let started_at = self.started_at_unix_ms.load(Ordering::Relaxed);
        if started_at == 0 {
            return 0;
        }
        now_unix_ms().saturating_sub(started_at)
    }

    pub fn begin(&self, total_wallets: usize, initial_stage: StartupStage) {
        let started_at_unix_ms = now_unix_ms();

        self.in_progress.store(true, Ordering::Relaxed);
        self.started_at_unix_ms
            .store(started_at_unix_ms, Ordering::Relaxed);
        self.final_elapsed_ms.store(0, Ordering::Relaxed);
        self.total_wallets.store(total_wallets, Ordering::Relaxed);
        self.wallets_created.store(0, Ordering::Relaxed);
        self.viewer_fvk_bundles_ready.store(0, Ordering::Relaxed);
        self.wallets_funded.store(0, Ordering::Relaxed);
        self.wallets_balance_ready.store(0, Ordering::Relaxed);
        self.deposits_submitted.store(0, Ordering::Relaxed);
        self.notes_indexed.store(0, Ordering::Relaxed);
        self.stage.store(initial_stage as u8, Ordering::Relaxed);

        tracing::info!(
            stage = initial_stage.label(),
            total_wallets,
            started_at_unix_ms,
            "Startup progress tracking started"
        );
    }

    pub fn set_total_wallets(&self, total_wallets: usize) {
        self.total_wallets.store(total_wallets, Ordering::Relaxed);
    }

    pub fn set_stage(&self, stage: StartupStage) {
        let previous = StartupStage::from_u8(self.stage.swap(stage as u8, Ordering::Relaxed));
        if previous == stage {
            return;
        }

        tracing::info!(
            previous_stage = previous.label(),
            stage = stage.label(),
            total_wallets = self.total_wallets(),
            elapsed_ms = self.elapsed_ms(),
            "Startup stage changed"
        );
    }

    fn counter(&self, counter: StartupCounter) -> &AtomicUsize {
        match counter {
            StartupCounter::WalletsCreated => &self.wallets_created,
            StartupCounter::ViewerFvkBundlesReady => &self.viewer_fvk_bundles_ready,
            StartupCounter::WalletsFunded => &self.wallets_funded,
            StartupCounter::WalletsBalanceReady => &self.wallets_balance_ready,
            StartupCounter::DepositsSubmitted => &self.deposits_submitted,
            StartupCounter::DepositNotesIndexed => &self.notes_indexed,
        }
    }

    pub fn set_progress(&self, stage: StartupStage, counter: StartupCounter, completed: usize) {
        if !self.is_in_progress() {
            return;
        }

        let total_wallets = self.total_wallets();
        let completed = completed.min(total_wallets);
        self.counter(counter).store(completed, Ordering::Relaxed);

        if should_log_progress(completed, total_wallets) {
            tracing::info!(
                stage = stage.label(),
                metric = counter.metric(),
                completed,
                total_wallets,
                progress_pct = progress_percent(completed, total_wallets),
                elapsed_ms = self.elapsed_ms(),
                "Startup progress"
            );
        }
    }

    pub fn increment_progress(&self, stage: StartupStage, counter: StartupCounter) {
        if !self.is_in_progress() {
            return;
        }

        let total_wallets = self.total_wallets();
        let metric_counter = self.counter(counter);
        let completed = metric_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let completed_capped = completed.min(total_wallets);
        if completed > total_wallets {
            metric_counter.store(total_wallets, Ordering::Relaxed);
        }

        if should_log_progress(completed_capped, total_wallets) {
            tracing::info!(
                stage = stage.label(),
                metric = counter.metric(),
                completed = completed_capped,
                total_wallets,
                progress_pct = progress_percent(completed_capped, total_wallets),
                elapsed_ms = self.elapsed_ms(),
                "Startup progress"
            );
        }
    }

    pub fn seed_restored_wallet_progress(&self, wallet_count: usize) {
        self.set_total_wallets(wallet_count);
        self.wallets_created.store(wallet_count, Ordering::Relaxed);
        self.wallets_funded.store(wallet_count, Ordering::Relaxed);
        self.wallets_balance_ready
            .store(wallet_count, Ordering::Relaxed);
        self.deposits_submitted
            .store(wallet_count, Ordering::Relaxed);
        self.notes_indexed.store(wallet_count, Ordering::Relaxed);
    }

    pub fn mark_complete(&self) {
        let elapsed_ms = self.elapsed_ms();
        self.final_elapsed_ms.store(elapsed_ms, Ordering::Relaxed);
        self.stage
            .store(StartupStage::Completed as u8, Ordering::Relaxed);
        self.in_progress.store(false, Ordering::Relaxed);

        tracing::info!(
            stage = StartupStage::Completed.label(),
            elapsed_ms,
            total_wallets = self.total_wallets(),
            wallets_created = self.wallets_created.load(Ordering::Relaxed),
            viewer_fvk_bundles_ready = self.viewer_fvk_bundles_ready.load(Ordering::Relaxed),
            wallets_funded = self.wallets_funded.load(Ordering::Relaxed),
            wallets_balance_ready = self.wallets_balance_ready.load(Ordering::Relaxed),
            deposits_submitted = self.deposits_submitted.load(Ordering::Relaxed),
            deposit_notes_indexed = self.notes_indexed.load(Ordering::Relaxed),
            "Startup progress completed"
        );
    }

    pub fn mark_failed(&self) {
        let elapsed_ms = self.elapsed_ms();
        self.final_elapsed_ms.store(elapsed_ms, Ordering::Relaxed);
        self.stage
            .store(StartupStage::Failed as u8, Ordering::Relaxed);
        self.in_progress.store(false, Ordering::Relaxed);

        tracing::error!(
            stage = StartupStage::Failed.label(),
            elapsed_ms,
            total_wallets = self.total_wallets(),
            wallets_created = self.wallets_created.load(Ordering::Relaxed),
            wallets_funded = self.wallets_funded.load(Ordering::Relaxed),
            deposits_submitted = self.deposits_submitted.load(Ordering::Relaxed),
            deposit_notes_indexed = self.notes_indexed.load(Ordering::Relaxed),
            "Startup progress marked as failed"
        );
    }

    pub fn snapshot(&self) -> StartupProgressResponse {
        let in_progress = self.is_in_progress();
        let elapsed_ms = if in_progress {
            self.elapsed_ms()
        } else {
            self.final_elapsed_ms.load(Ordering::Relaxed)
        };

        StartupProgressResponse {
            in_progress,
            stage: StartupStage::from_u8(self.stage.load(Ordering::Relaxed)).label(),
            total_wallets: self.total_wallets(),
            wallets_created: self.wallets_created.load(Ordering::Relaxed),
            viewer_fvk_bundles_ready: self.viewer_fvk_bundles_ready.load(Ordering::Relaxed),
            wallets_funded: self.wallets_funded.load(Ordering::Relaxed),
            wallets_balance_ready: self.wallets_balance_ready.load(Ordering::Relaxed),
            deposits_submitted: self.deposits_submitted.load(Ordering::Relaxed),
            deposit_notes_indexed: self.notes_indexed.load(Ordering::Relaxed),
            elapsed_ms,
        }
    }
}

pub struct ScaleUpProgressTracker {
    in_progress: AtomicBool,
    stage: AtomicU8,
    started_at_unix_ms: AtomicU64,
    final_elapsed_ms: AtomicU64,
    start_wallets: AtomicUsize,
    target_wallets: AtomicUsize,
    batch_size: AtomicUsize,
    wallets_created: AtomicUsize,
    viewer_fvk_bundles_ready: AtomicUsize,
    wallets_funded: AtomicUsize,
    wallets_balance_ready: AtomicUsize,
    deposits_submitted: AtomicUsize,
    notes_indexed: AtomicUsize,
    wallets_appended: AtomicUsize,
}

impl ScaleUpProgressTracker {
    pub fn new(initial_target_wallets: usize) -> Self {
        Self {
            in_progress: AtomicBool::new(false),
            stage: AtomicU8::new(ScaleUpStage::Idle as u8),
            started_at_unix_ms: AtomicU64::new(0),
            final_elapsed_ms: AtomicU64::new(0),
            start_wallets: AtomicUsize::new(initial_target_wallets),
            target_wallets: AtomicUsize::new(initial_target_wallets),
            batch_size: AtomicUsize::new(0),
            wallets_created: AtomicUsize::new(0),
            viewer_fvk_bundles_ready: AtomicUsize::new(0),
            wallets_funded: AtomicUsize::new(0),
            wallets_balance_ready: AtomicUsize::new(0),
            deposits_submitted: AtomicUsize::new(0),
            notes_indexed: AtomicUsize::new(0),
            wallets_appended: AtomicUsize::new(0),
        }
    }

    pub fn is_in_progress(&self) -> bool {
        self.in_progress.load(Ordering::Relaxed)
    }

    fn start_wallets(&self) -> usize {
        self.start_wallets.load(Ordering::Relaxed)
    }

    fn target_wallets(&self) -> usize {
        self.target_wallets.load(Ordering::Relaxed)
    }

    fn elapsed_ms(&self) -> u64 {
        let started_at = self.started_at_unix_ms.load(Ordering::Relaxed);
        if started_at == 0 {
            return 0;
        }
        now_unix_ms().saturating_sub(started_at)
    }

    fn total_needed(&self) -> usize {
        self.target_wallets().saturating_sub(self.start_wallets())
    }

    pub fn set_target_wallets(&self, target_wallets: usize) {
        self.target_wallets.store(target_wallets, Ordering::Relaxed);
    }

    pub fn reset_when_idle(&self, current_wallets: usize) {
        self.start_wallets.store(current_wallets, Ordering::Relaxed);
        self.stage
            .store(ScaleUpStage::Idle as u8, Ordering::Relaxed);
        self.batch_size.store(0, Ordering::Relaxed);
        self.wallets_created.store(0, Ordering::Relaxed);
        self.viewer_fvk_bundles_ready.store(0, Ordering::Relaxed);
        self.wallets_funded.store(0, Ordering::Relaxed);
        self.wallets_balance_ready.store(0, Ordering::Relaxed);
        self.deposits_submitted.store(0, Ordering::Relaxed);
        self.notes_indexed.store(0, Ordering::Relaxed);
        self.wallets_appended.store(0, Ordering::Relaxed);
        self.final_elapsed_ms.store(0, Ordering::Relaxed);
    }

    pub fn begin(&self, start_wallets: usize, target_wallets: usize) {
        let started_at_unix_ms = now_unix_ms();
        let wallets_needed = target_wallets.saturating_sub(start_wallets);

        self.in_progress.store(true, Ordering::Relaxed);
        self.stage.store(
            ScaleUpStage::WaitingForSequencerReady as u8,
            Ordering::Relaxed,
        );
        self.started_at_unix_ms
            .store(started_at_unix_ms, Ordering::Relaxed);
        self.final_elapsed_ms.store(0, Ordering::Relaxed);
        self.start_wallets.store(start_wallets, Ordering::Relaxed);
        self.target_wallets.store(target_wallets, Ordering::Relaxed);
        self.batch_size.store(0, Ordering::Relaxed);
        self.wallets_created.store(0, Ordering::Relaxed);
        self.viewer_fvk_bundles_ready.store(0, Ordering::Relaxed);
        self.wallets_funded.store(0, Ordering::Relaxed);
        self.wallets_balance_ready.store(0, Ordering::Relaxed);
        self.deposits_submitted.store(0, Ordering::Relaxed);
        self.notes_indexed.store(0, Ordering::Relaxed);
        self.wallets_appended.store(0, Ordering::Relaxed);

        tracing::info!(
            stage = ScaleUpStage::WaitingForSequencerReady.label(),
            start_wallets,
            target_wallets,
            wallets_needed,
            started_at_unix_ms,
            "Background wallet scale-up tracking started"
        );
    }

    pub fn update_target(&self, target_wallets: usize) {
        let previous_target = self.target_wallets.swap(target_wallets, Ordering::Relaxed);
        if previous_target == target_wallets {
            return;
        }

        tracing::info!(
            previous_target_wallets = previous_target,
            target_wallets,
            start_wallets = self.start_wallets(),
            wallets_needed = self.total_needed(),
            elapsed_ms = self.elapsed_ms(),
            "Background wallet scale-up target updated"
        );
    }

    pub fn set_stage(&self, stage: ScaleUpStage) {
        let previous = ScaleUpStage::from_u8(self.stage.swap(stage as u8, Ordering::Relaxed));
        if previous == stage {
            return;
        }

        tracing::info!(
            previous_stage = previous.label(),
            stage = stage.label(),
            target_wallets = self.target_wallets(),
            wallets_appended = self.wallets_appended.load(Ordering::Relaxed),
            wallets_needed = self.total_needed(),
            elapsed_ms = self.elapsed_ms(),
            "Background wallet scale-up stage changed"
        );
    }

    pub fn set_batch_size(&self, batch_size: usize) {
        self.batch_size.store(batch_size, Ordering::Relaxed);
    }

    fn counter(&self, counter: ScaleUpCounter) -> &AtomicUsize {
        match counter {
            ScaleUpCounter::WalletsCreated => &self.wallets_created,
            ScaleUpCounter::ViewerFvkBundlesReady => &self.viewer_fvk_bundles_ready,
            ScaleUpCounter::WalletsFunded => &self.wallets_funded,
            ScaleUpCounter::WalletsBalanceReady => &self.wallets_balance_ready,
            ScaleUpCounter::DepositsSubmitted => &self.deposits_submitted,
            ScaleUpCounter::DepositNotesIndexed => &self.notes_indexed,
            ScaleUpCounter::WalletsAppended => &self.wallets_appended,
        }
    }

    pub fn increment_progress(&self, stage: ScaleUpStage, counter: ScaleUpCounter) {
        if !self.is_in_progress() {
            return;
        }

        let total_needed = self.total_needed();
        let metric_counter = self.counter(counter);
        let completed = metric_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let completed_capped = completed.min(total_needed);
        if completed > total_needed {
            metric_counter.store(total_needed, Ordering::Relaxed);
        }

        if should_log_progress(completed_capped, total_needed) {
            tracing::info!(
                stage = stage.label(),
                metric = counter.metric(),
                completed = completed_capped,
                total_needed,
                progress_pct = progress_percent(completed_capped, total_needed),
                elapsed_ms = self.elapsed_ms(),
                "Background wallet scale-up progress"
            );
        }
    }

    pub fn mark_failed(&self) {
        let elapsed_ms = self.elapsed_ms();
        self.stage
            .store(ScaleUpStage::Failed as u8, Ordering::Relaxed);

        tracing::warn!(
            stage = ScaleUpStage::Failed.label(),
            elapsed_ms,
            start_wallets = self.start_wallets(),
            target_wallets = self.target_wallets(),
            wallets_appended = self.wallets_appended.load(Ordering::Relaxed),
            wallets_needed = self.total_needed(),
            "Background wallet scale-up batch failed; will retry"
        );
    }

    pub fn mark_complete(&self, current_wallets: usize) {
        let elapsed_ms = self.elapsed_ms();
        self.final_elapsed_ms.store(elapsed_ms, Ordering::Relaxed);
        self.stage
            .store(ScaleUpStage::Completed as u8, Ordering::Relaxed);
        self.in_progress.store(false, Ordering::Relaxed);
        self.batch_size.store(0, Ordering::Relaxed);

        tracing::info!(
            stage = ScaleUpStage::Completed.label(),
            elapsed_ms,
            current_wallets,
            start_wallets = self.start_wallets(),
            target_wallets = self.target_wallets(),
            wallets_needed = self.total_needed(),
            wallets_created = self.wallets_created.load(Ordering::Relaxed),
            viewer_fvk_bundles_ready = self.viewer_fvk_bundles_ready.load(Ordering::Relaxed),
            wallets_funded = self.wallets_funded.load(Ordering::Relaxed),
            wallets_balance_ready = self.wallets_balance_ready.load(Ordering::Relaxed),
            deposits_submitted = self.deposits_submitted.load(Ordering::Relaxed),
            deposit_notes_indexed = self.notes_indexed.load(Ordering::Relaxed),
            wallets_appended = self.wallets_appended.load(Ordering::Relaxed),
            "Background wallet scale-up completed"
        );
    }

    pub fn snapshot(&self, current_wallets: usize) -> ScaleUpProgressResponse {
        let in_progress = self.is_in_progress();
        let stage = ScaleUpStage::from_u8(self.stage.load(Ordering::Relaxed)).label();
        let start_wallets = self.start_wallets();
        let target_wallets = self.target_wallets();
        let wallets_needed = target_wallets.saturating_sub(start_wallets);
        let wallets_remaining = target_wallets.saturating_sub(current_wallets);
        let elapsed_ms = if in_progress {
            self.elapsed_ms()
        } else {
            self.final_elapsed_ms.load(Ordering::Relaxed)
        };

        ScaleUpProgressResponse {
            in_progress,
            stage,
            start_wallets,
            target_wallets,
            current_wallets,
            wallets_needed,
            wallets_remaining,
            current_batch_size: self.batch_size.load(Ordering::Relaxed),
            wallets_created: self.wallets_created.load(Ordering::Relaxed),
            viewer_fvk_bundles_ready: self.viewer_fvk_bundles_ready.load(Ordering::Relaxed),
            wallets_funded: self.wallets_funded.load(Ordering::Relaxed),
            wallets_balance_ready: self.wallets_balance_ready.load(Ordering::Relaxed),
            deposits_submitted: self.deposits_submitted.load(Ordering::Relaxed),
            deposit_notes_indexed: self.notes_indexed.load(Ordering::Relaxed),
            wallets_appended: self.wallets_appended.load(Ordering::Relaxed),
            elapsed_ms,
        }
    }
}

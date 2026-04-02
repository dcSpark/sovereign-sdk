use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use borsh::BorshDeserialize;
use demo_stf::runtime::{Runtime, RuntimeCall};
use full_node_configs::sequencer::SeqConfigExtension;
use hex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sov_bank::{config_gas_token_id, CallMessage as BankCallMessage, Coins, TokenId};
use sov_cli::wallet_state::PrivateKeyAndAddress;
use sov_db::accessory_db::AccessoryDb;
use sov_evm::EvmAuthenticatorInput;
use sov_midnight_da::storable::service::StorableMidnightDaService;
use sov_modules_api::capabilities::UniquenessData;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::rest::utils::ErrorObject;
use sov_modules_api::runtime::capabilities::authentication::{
    calculate_hash, config_chain_id, TransactionAuthenticator,
};
use sov_modules_api::transaction::TxDetails;
use sov_modules_api::transaction::{PriorityFeeBips, Transaction, UnsignedTransaction};
use sov_modules_api::FullyBakedTx;
use sov_modules_api::{Amount, CredentialId, PrivateKey, PublicKey, RawTx, Spec};
use sov_rollup_interface::common::SlotNumber;
use sov_rollup_interface::TxHash;
use sov_sequencer::{Sequencer, SequencerNotReadyDetails};
use tokio::fs;
use tokio::task::JoinHandle;
use tokio::time::interval;
use tracing::{debug, info, warn};

use rockbound::cache::delta_reader::DeltaReader;
use rockbound::DB;

use crate::MockRollupSpec;
use sov_modules_stf_blueprint::Runtime as StfRuntime;

use sov_midnight_adapter::{MidnightDeposit, MidnightIndexerClient};

type BridgeSpec = MockRollupSpec<Native>;
type BridgeRuntime = Runtime<BridgeSpec>;
type BridgeAuthenticator = <BridgeRuntime as StfRuntime<BridgeSpec>>::Auth;

pub(crate) struct BridgeCursorStore {
    db: Arc<DB>,
    accessor: AccessoryDb,
    key: Vec<u8>,
}

impl BridgeCursorStore {
    const KEY_BYTES: &'static [u8] = b"midnight_bridge.cursor";
    const WITHDRAWAL_RELAY_KEY: &'static [u8] = b"midnight_bridge.withdrawal_relay_cursor";
    const CURSOR_SUBDIR: &'static str = "midnight_bridge_cursor";

    pub(crate) fn open(storage_path: &Path) -> Result<Self> {
        let cursor_path = storage_path.join(Self::CURSOR_SUBDIR);
        std::fs::create_dir_all(&cursor_path).with_context(|| {
            format!(
                "Failed to create Midnight bridge cursor directory at {}",
                cursor_path.display()
            )
        })?;
        let db = Arc::new(
            AccessoryDb::get_rockbound_options()
                .default_setup_db_in_path(&cursor_path)
                .with_context(|| {
                    format!(
                        "Failed to open accessory DB for Midnight bridge cursor at {}",
                        cursor_path.display()
                    )
                })?,
        );
        let reader = DeltaReader::new(db.clone(), Vec::new());
        let accessor = AccessoryDb::with_reader(reader)
            .context("Failed to create accessory DB reader for Midnight bridge cursor")?;
        Ok(Self {
            db,
            accessor,
            key: Self::KEY_BYTES.to_vec(),
        })
    }

    fn load_cursor(&self) -> Result<Option<u64>> {
        let raw = self
            .accessor
            .get_value_option(&self.key, SlotNumber::GENESIS)
            .context("Failed to read Midnight bridge cursor from accessory DB")?;
        match raw {
            Some(bytes) => {
                anyhow::ensure!(
                    bytes.len() == 8,
                    "Midnight bridge cursor payload must be 8 bytes, got {}",
                    bytes.len()
                );
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&bytes);
                Ok(Some(u64::from_le_bytes(arr)))
            }
            None => Ok(None),
        }
    }

    fn persist_cursor(&self, cursor: u64) -> Result<()> {
        let bytes = cursor.to_le_bytes().to_vec();
        let batch = AccessoryDb::materialize_values(
            vec![(self.key.clone(), Some(bytes))],
            SlotNumber::GENESIS,
        )?;
        self.db
            .write_schemas(batch)
            .context("Failed to persist Midnight bridge cursor")
    }

    fn load_withdrawal_relay_cursor(&self) -> Result<Option<u64>> {
        let raw = self
            .accessor
            .get_value_option(&Self::WITHDRAWAL_RELAY_KEY.to_vec(), SlotNumber::GENESIS)
            .context("Failed to read withdrawal relay cursor from accessory DB")?;
        match raw {
            Some(bytes) => {
                anyhow::ensure!(
                    bytes.len() == 8,
                    "Withdrawal relay cursor payload must be 8 bytes, got {}",
                    bytes.len()
                );
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&bytes);
                Ok(Some(u64::from_le_bytes(arr)))
            }
            None => Ok(None),
        }
    }

    fn persist_withdrawal_relay_cursor(&self, cursor: u64) -> Result<()> {
        let bytes = cursor.to_le_bytes().to_vec();
        let batch = AccessoryDb::materialize_values(
            vec![(Self::WITHDRAWAL_RELAY_KEY.to_vec(), Some(bytes))],
            SlotNumber::GENESIS,
        )?;
        self.db
            .write_schemas(batch)
            .context("Failed to persist withdrawal relay cursor")
    }
}

struct BridgeConfig {
    runtime: RuntimeBridgeSettings,
    deposit_source: DepositSource,
}

enum DepositSource {
    Mock(MockDepositSource),
    Indexer(IndexerDepositSource),
}

struct MockDepositSource {
    events_path: PathBuf,
}

impl MockDepositSource {
    fn path(&self) -> &Path {
        &self.events_path
    }
}

struct IndexerDepositSource {
    client: Arc<MidnightIndexerClient>,
    start_deposit_index: Option<u64>,
}

impl IndexerDepositSource {
    fn client(&self) -> &MidnightIndexerClient {
        &self.client
    }

    fn start_deposit_index(&self) -> Option<u64> {
        self.start_deposit_index
    }

    fn client_arc(&self) -> Arc<MidnightIndexerClient> {
        Arc::clone(&self.client)
    }
}

#[async_trait]
pub(crate) trait BridgeSequencer: Send + Sync + 'static {
    async fn accept_bridge_tx(&self, tx: FullyBakedTx) -> std::result::Result<TxHash, ErrorObject>;
    async fn readiness_status(&self) -> std::result::Result<(), SequencerNotReadyDetails>;
}

#[async_trait]
impl<Seq> BridgeSequencer for Seq
where
    Seq: Sequencer<Spec = BridgeSpec, Rt = BridgeRuntime, Da = StorableMidnightDaService>,
{
    async fn accept_bridge_tx(&self, tx: FullyBakedTx) -> std::result::Result<TxHash, ErrorObject> {
        let accepted = Sequencer::accept_tx(self, tx).await?;
        Ok(accepted.tx_hash)
    }

    async fn readiness_status(&self) -> std::result::Result<(), SequencerNotReadyDetails> {
        Sequencer::is_ready(self).await
    }
}

/// Spawns the Midnight bridge background task when enabled via `sequencer.extension.midnight_bridge`.
///
/// `resolved_contract_address` is the contract address that was deployed or loaded during bridge
/// lifecycle startup. When provided it takes precedence over the config value.
///
/// `rollup_dedup_url` should be the rollup's HTTP base URL (e.g. `http://127.0.0.1:12346`). When set,
/// the bridge fetches the next generation for its credential from the rollup's dedup API at startup,
/// so credit transactions pass the uniqueness check. If omitted, the bridge uses generation 0, which
/// will fail if the bridge credential has already been used on the rollup.
pub(crate) fn spawn_midnight_bridge<Seq>(
    sequencer: Arc<Seq>,
    extension: &SeqConfigExtension,
    cursor_store: Option<BridgeCursorStore>,
    resolved_contract_address: Option<&str>,
    rollup_dedup_url: Option<String>,
) -> Result<Option<JoinHandle<anyhow::Result<()>>>>
where
    Seq: BridgeSequencer,
{
    let Some(config) = load_runtime_settings(extension, resolved_contract_address)? else {
        debug!("Midnight bridge disabled");
        return Ok(None);
    };

    match &config.deposit_source {
        DepositSource::Mock(mock) => {
            info!(
                poll_interval_ms = config.runtime.poll_interval.as_millis() as u64,
                path = %mock.path().display(),
                "Starting Midnight bridge background task (mock source)",
            );
        }
        DepositSource::Indexer(indexer) => {
            info!(
                poll_interval_ms = config.runtime.poll_interval.as_millis() as u64,
                indexer_http = %indexer.client().endpoint(),
                contract_address = %indexer.client().contract_address(),
                start_deposit_index = indexer.start_deposit_index(),
                "Starting Midnight bridge background task (Midnight indexer source)",
            );
        }
    }

    let executor_base_url = extension
        .midnight_bridge
        .as_ref()
        .map(|b| format!("http://127.0.0.1:{}", b.executor_port));

    let BridgeConfig {
        runtime,
        deposit_source,
    } = config;

    let bridge = MidnightBridge::new(
        sequencer,
        runtime,
        deposit_source,
        cursor_store,
        rollup_dedup_url,
        executor_base_url,
    )?;
    Ok(Some(tokio::spawn(async move { bridge.run().await })))
}

fn load_runtime_settings(
    extension: &SeqConfigExtension,
    resolved_contract_address: Option<&str>,
) -> Result<Option<BridgeConfig>> {
    let Some(raw) = extension.midnight_bridge.as_ref() else {
        info!("Midnight bridge disabled: missing `[sequencer.extension.midnight_bridge]` block");
        return Ok(None);
    };

    let signing_key =
        PrivateKeyAndAddress::<BridgeSpec>::from_json_file(&raw.signing_key_path, false)
            .with_context(|| {
                format!(
                    "Failed to read Midnight bridge signing key from {}",
                    raw.signing_key_path.display()
                )
            })?;

    let token_id = if let Some(token_id_bech32) = &raw.token_id_bech32 {
        TokenId::from_str(token_id_bech32).with_context(|| {
            format!(
                "Failed to parse Midnight bridge token id from {}",
                token_id_bech32
            )
        })?
    } else {
        config_gas_token_id()
    };

    let max_fee = Amount::from(raw.max_fee);
    if max_fee == Amount::ZERO {
        bail!("Midnight bridge max_fee must be greater than zero");
    }

    let poll_interval = Duration::from_millis(raw.poll_interval_ms.max(1));

    let deposit_source = if let Some(path) = raw.mock_events_path.clone() {
        DepositSource::Mock(MockDepositSource { events_path: path })
    } else {
        let indexer_http = raw.indexer_http.clone().ok_or_else(|| {
            anyhow!("indexer_http must be provided when mock_events_path is not configured")
        })?;
        let contract_address = resolved_contract_address
            .map(String::from)
            .or_else(|| raw.contract_address.clone())
            .ok_or_else(|| {
                anyhow!("contract_address must be provided when mock_events_path is not configured")
            })?;
        validate_contract_address(&contract_address)?;

        let timeout = Duration::from_millis(raw.indexer_timeout_ms.max(1));
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .context("Failed to build Midnight indexer HTTP client")?;

        let start_deposit_index = raw.start_deposit_index;

        let indexer = Arc::new(MidnightIndexerClient::new(
            client,
            indexer_http,
            contract_address,
        ));
        DepositSource::Indexer(IndexerDepositSource {
            client: indexer,
            start_deposit_index,
        })
    };

    Ok(Some(BridgeConfig {
        runtime: RuntimeBridgeSettings {
            signing_key,
            poll_interval,
            token_id,
            max_fee,
        },
        deposit_source,
    }))
}

struct RuntimeBridgeSettings {
    signing_key: PrivateKeyAndAddress<BridgeSpec>,
    poll_interval: Duration,
    token_id: TokenId,
    max_fee: Amount,
}

/// Response from the rollup dedup endpoint when using `?select=generation`.
#[derive(Debug, Deserialize)]
struct DedupGenerationResponse {
    #[serde(default)]
    generation: Option<u64>,
}

/// Response from `GET /modules/midnight-withdrawals/withdrawals/queue`.
#[derive(Debug, Deserialize)]
struct WithdrawalQueueStatusResponse {
    next_nonce: u64,
}

/// Response from `GET /modules/midnight-withdrawals/withdrawals/{nonce}/proof`.
#[derive(Debug, Deserialize)]
struct WithdrawalProofResponse {
    sender_bytes_hex: String,
    recipient_bytes_hex: String,
    amount: String,
    l1_proof: L1ProofResponse,
}

/// The `l1_proof` sub-object within [`WithdrawalProofResponse`].
#[derive(Debug, Deserialize)]
struct L1ProofResponse {
    batch_index: u64,
    nonce: u64,
    index_bits_le: Vec<bool>,
    sibling_hashes_hex: Vec<String>,
}

/// Partial response from the executor's `GET /state` endpoint.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecutorStateResponse {
    #[serde(default)]
    last_finalized_batch_index: Option<String>,
}

struct MidnightBridge<Seq> {
    sequencer: Arc<Seq>,
    settings: RuntimeBridgeSettings,
    deposit_source: DepositSource,
    processed_event_ids: HashSet<String>,
    next_generation: u64,
    idle_notice_sent: bool,
    next_chain_index: Option<u64>,
    cursor_store: Option<BridgeCursorStore>,
    /// When set, the bridge syncs next_generation from this rollup URL at startup.
    rollup_dedup_url: Option<String>,
    /// Next withdrawal nonce to relay to L1 (all nonces below this have been relayed).
    next_relay_nonce: u64,
    /// Base URL for the managed executor HTTP service (e.g. `http://127.0.0.1:3001`).
    executor_base_url: Option<String>,
}

impl<Seq> MidnightBridge<Seq>
where
    Seq: BridgeSequencer,
{
    fn new(
        sequencer: Arc<Seq>,
        settings: RuntimeBridgeSettings,
        deposit_source: DepositSource,
        mut cursor_store: Option<BridgeCursorStore>,
        rollup_dedup_url: Option<String>,
        executor_base_url: Option<String>,
    ) -> Result<Self> {
        let mut restored_cursor = None;

        if matches!(deposit_source, DepositSource::Indexer(_)) {
            if let Some(store) = cursor_store.as_ref() {
                match store.load_cursor() {
                    Ok(Some(cursor)) => {
                        info!(cursor, "Midnight bridge restored cursor from accessory DB");
                        restored_cursor = Some(cursor);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!(error = ?err, "Midnight bridge failed to read cursor from accessory DB");
                    }
                }
            } else {
                warn!(
                    "Midnight bridge cursor persistence disabled: accessory DB handle unavailable"
                );
            }
        } else {
            cursor_store = None;
        }

        let next_chain_index = match &deposit_source {
            DepositSource::Indexer(source) => {
                restored_cursor.or(source.start_deposit_index()).or(Some(0))
            }
            DepositSource::Mock(_) => None,
        };

        // Restore the withdrawal relay cursor from persistent storage.
        let next_relay_nonce = cursor_store
            .as_ref()
            .and_then(|store| match store.load_withdrawal_relay_cursor() {
                Ok(Some(cursor)) => {
                    info!(cursor, "Midnight bridge restored withdrawal relay cursor");
                    Some(cursor)
                }
                Ok(None) => None,
                Err(err) => {
                    warn!(error = ?err, "Failed to read withdrawal relay cursor");
                    None
                }
            })
            .unwrap_or(0);

        let bridge = Self {
            sequencer,
            settings,
            deposit_source,
            processed_event_ids: HashSet::new(),
            next_generation: 0,
            idle_notice_sent: false,
            next_chain_index,
            cursor_store,
            rollup_dedup_url,
            next_relay_nonce,
            executor_base_url,
        };

        if restored_cursor.is_none() {
            if let Some(cursor) = bridge.next_chain_index {
                bridge.persist_cursor(cursor);
            }
        }

        Ok(bridge)
    }

    fn persist_cursor(&self, cursor: u64) {
        if let Some(store) = &self.cursor_store {
            if let Err(err) = store.persist_cursor(cursor) {
                warn!(value = cursor, error = ?err, "Midnight bridge failed to persist cursor");
            }
        }
    }

    fn set_cursor(&mut self, cursor: u64) {
        self.next_chain_index = Some(cursor);
        self.persist_cursor(cursor);
    }

    fn advance_relay_cursor(&mut self, next_nonce: u64) {
        self.next_relay_nonce = next_nonce;
        if let Some(store) = &self.cursor_store {
            if let Err(err) = store.persist_withdrawal_relay_cursor(next_nonce) {
                warn!(
                    value = next_nonce,
                    error = ?err,
                    "Failed to persist withdrawal relay cursor"
                );
            }
        }
    }

    /// Relays pending L2→L1 withdrawal proofs to the executor service.
    ///
    /// For each unrelayed nonce, fetches the Merkle proof from the rollup REST API and
    /// POSTs it to the executor's `relay-withdraw-night-with-proof` endpoint. The relay
    /// cursor is persisted after each successful relay so progress survives restarts.
    async fn relay_pending_withdrawals(&mut self) -> Result<()> {
        let executor_url = match &self.executor_base_url {
            Some(url) => url.clone(),
            None => return Ok(()),
        };
        let rollup_url = match &self.rollup_dedup_url {
            Some(url) => url.trim_end_matches('/').to_string(),
            None => return Ok(()),
        };

        // 1. Get last finalized batch index from the executor.
        let state_url = format!("{}/state", executor_url.trim_end_matches('/'));
        let state_resp: ExecutorStateResponse = match reqwest::get(&state_url).await {
            Ok(resp) if resp.status().is_success() => resp
                .json()
                .await
                .context("Failed to parse executor /state response")?,
            Ok(resp) => {
                debug!(
                    status = %resp.status(),
                    "Executor /state returned non-success; skipping withdrawal relay"
                );
                return Ok(());
            }
            Err(err) => {
                debug!(error = ?err, "Executor /state unreachable; skipping withdrawal relay");
                return Ok(());
            }
        };

        let finalized_batch_index: u64 = state_resp
            .last_finalized_batch_index
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        if finalized_batch_index == 0 {
            return Ok(());
        }

        // 2. Get the withdrawal queue status from the rollup.
        let queue_url = format!(
            "{}/modules/midnight-withdrawals/withdrawals/queue",
            rollup_url
        );
        let queue_resp: WithdrawalQueueStatusResponse = reqwest::get(&queue_url)
            .await
            .context("Failed to fetch withdrawal queue status")?
            .json()
            .await
            .context("Failed to parse withdrawal queue status")?;

        let next_nonce = queue_resp.next_nonce;
        if next_nonce <= self.next_relay_nonce {
            return Ok(());
        }

        // 3. Relay each pending withdrawal.
        let client = reqwest::Client::new();
        for nonce in self.next_relay_nonce..next_nonce {
            let proof_url = format!(
                "{}/modules/midnight-withdrawals/withdrawals/{}/proof?batch_index={}",
                rollup_url, nonce, finalized_batch_index
            );
            let proof: WithdrawalProofResponse = match reqwest::get(&proof_url).await {
                Ok(resp) if resp.status().is_success() => resp
                    .json()
                    .await
                    .context("Failed to parse withdrawal proof response")?,
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    warn!(
                        nonce,
                        status = %status,
                        body = %body,
                        "Failed to fetch withdrawal proof; will retry next cycle"
                    );
                    break;
                }
                Err(err) => {
                    warn!(nonce, error = ?err, "Failed to fetch withdrawal proof; will retry next cycle");
                    break;
                }
            };

            let relay_url = format!(
                "{}/relay-withdraw-night-with-proof",
                executor_url.trim_end_matches('/')
            );
            let relay_body = serde_json::json!({
                "l2Sender": proof.sender_bytes_hex,
                "recipient": proof.recipient_bytes_hex,
                "amount": proof.amount,
                "batchIndex": proof.l1_proof.batch_index.to_string(),
                "nonce": proof.l1_proof.nonce.to_string(),
                "indexBits": proof.l1_proof.index_bits_le,
                "siblings": proof.l1_proof.sibling_hashes_hex,
            });

            match client.post(&relay_url).json(&relay_body).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!(
                        nonce,
                        batch_index = finalized_batch_index,
                        amount = %proof.amount,
                        "Relayed withdrawal proof to L1 executor"
                    );
                    self.advance_relay_cursor(nonce + 1);
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    if body.contains("ALREADY_EXECUTED") {
                        info!(
                            nonce,
                            "Withdrawal already executed on L1; advancing cursor"
                        );
                        self.advance_relay_cursor(nonce + 1);
                        continue;
                    }
                    warn!(
                        nonce,
                        status = %status,
                        body = %body,
                        "Withdrawal relay failed; will retry next cycle"
                    );
                    break;
                }
                Err(err) => {
                    warn!(nonce, error = ?err, "Withdrawal relay request failed; will retry next cycle");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Fetches the next generation for the bridge credential from the rollup dedup API
    /// and sets `next_generation` so credit transactions pass the uniqueness check.
    /// Retries on connection errors so the rollup HTTP server has time to start.
    async fn sync_next_generation_from_rollup(&mut self) {
        const MAX_DEDUP_RETRIES: u32 = 10;
        const RETRY_DELAY_MS: u64 = 500;

        let Some(ref base_url) = self.rollup_dedup_url else {
            debug!(
                "Midnight bridge has no rollup_dedup_url; using next_generation=0 (may fail if credential already used)"
            );
            return;
        };

        let credential_id = self.settings.signing_key.private_key.pub_key().credential_id();
        let url = format!(
            "{}/rollup/addresses/{}/dedup?select=generation",
            base_url.trim_end_matches('/'),
            credential_id
        );

        for attempt in 1..=MAX_DEDUP_RETRIES {
            match reqwest::get(&url).await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<DedupGenerationResponse>().await {
                        Ok(decoded) => {
                            if let Some(gen) = decoded.generation {
                                // Never decrease: we may have already incremented locally after
                                // submitting a credit that isn’t reflected in rollup state yet.
                                self.next_generation = self.next_generation.max(gen);
                                debug!(
                                    credential_id = %credential_id,
                                    next_generation = self.next_generation,
                                    "Midnight bridge synced next_generation from rollup dedup"
                                );
                            } else {
                                warn!(
                                    url = %url,
                                    "Rollup dedup response had no generation field; using next_generation=0"
                                );
                            }
                        }
                        Err(err) => {
                            warn!(
                                url = %url,
                                error = ?err,
                                "Midnight bridge failed to parse dedup response; using next_generation=0"
                            );
                        }
                    }
                    return;
                }
                Ok(resp) => {
                    warn!(
                        url = %url,
                        status = %resp.status(),
                        "Midnight bridge dedup request failed; using next_generation=0"
                    );
                    return;
                }
                Err(err) => {
                    let is_connect_err = err.is_connect();
                    if is_connect_err && attempt < MAX_DEDUP_RETRIES {
                        debug!(
                            url = %url,
                            attempt,
                            max = MAX_DEDUP_RETRIES,
                            "Rollup not ready, retrying dedup fetch"
                        );
                        tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                    } else {
                        warn!(
                            url = %url,
                            error = ?err,
                            "Midnight bridge failed to fetch dedup; using next_generation=0"
                        );
                        return;
                    }
                }
            }
        }
    }

    async fn run(mut self) -> Result<()> {
        let mut ticker = interval(self.settings.poll_interval);
        enum PollRequest {
            Mock(PathBuf),
            Indexer(Arc<MidnightIndexerClient>),
        }
        loop {
            ticker.tick().await;

            // Re-sync next_generation each poll so we see current rollup state (initial sync
            // can run before state has this credential, giving 0 and then uniqueness failures).
            self.sync_next_generation_from_rollup().await;

            let request = match &self.deposit_source {
                DepositSource::Mock(source) => PollRequest::Mock(source.path().to_path_buf()),
                DepositSource::Indexer(source) => PollRequest::Indexer(source.client_arc()),
            };

            let poll_result = match request {
                PollRequest::Mock(path) => {
                    let events = read_deposit_file(&path).await?;
                    self.process_mock_events(events, &path).await;
                    Ok(())
                }
                PollRequest::Indexer(client) => self.poll_chain(client.as_ref()).await,
            };

            if let Err(err) = poll_result {
                warn!(error = ?err, "Midnight bridge failed to fetch deposits");
            }

            // Relay pending L2→L1 withdrawal proofs to the executor.
            if let Err(err) = self.relay_pending_withdrawals().await {
                warn!(error = ?err, "Withdrawal relay cycle failed");
            }
        }
    }

    async fn process_mock_events(&mut self, events: Vec<Deposit>, events_path: &Path) {
        if events.is_empty() {
            if !self.idle_notice_sent {
                info!(
                    path = %events_path.display(),
                    "Midnight bridge idle: no mock events detected",
                );
                self.idle_notice_sent = true;
            }
            return;
        }

        if let Err(details) = self.sequencer.readiness_status().await {
            debug!(?details, "Midnight bridge waiting for sequencer readiness");
            return;
        }

        self.idle_notice_sent = false;

        debug!(count = events.len(), "Midnight bridge fetched events");

        for deposit in &events {
            let event_id = deposit.event_id();
            if self.processed_event_ids.contains(&event_id) {
                continue;
            }

            match self.submit_credit(&event_id, deposit, None).await {
                Ok(()) => {
                    self.processed_event_ids.insert(event_id);
                }
                Err(err) => {
                    warn!(
                        event_id = %event_id,
                        nonce = deposit.nonce,
                        error = ?err,
                        "Midnight bridge failed to submit credit",
                    );
                }
            }
        }
    }

    async fn poll_chain(&mut self, client: &MidnightIndexerClient) -> Result<()> {
        let ledger = client.snapshot().await?;
        let rollup = &ledger.rollup;

        let latest = rollup.next_cross_domain_message_index;
        let cursor = match self.next_chain_index {
            Some(index) => index,
            None => {
                let start = latest.saturating_sub(1);
                self.set_cursor(start);
                info!(
                    indexer_http = %client.endpoint(),
                    contract_address = %client.contract_address(),
                    latest_index = latest,
                    start_deposit_index = start,
                    "Midnight bridge synchronized cursor to Midnight deposits",
                );
                start
            }
        };

        if cursor > latest {
            warn!(
                cursor = cursor,
                latest = latest,
                "Midnight bridge cursor ahead of on-chain index; rewinding",
            );
            self.set_cursor(latest);
            return Ok(());
        }

        if cursor == latest {
            if !self.idle_notice_sent {
                info!(
                    indexer_http = %client.endpoint(),
                    contract_address = %client.contract_address(),
                    cursor = cursor,
                    "Midnight bridge idle: no new Midnight deposits",
                );
                self.idle_notice_sent = true;
            }
            return Ok(());
        }

        if let Err(details) = self.sequencer.readiness_status().await {
            debug!(?details, "Midnight bridge waiting for sequencer readiness");
            return Ok(());
        }

        self.idle_notice_sent = false;

        for index in cursor..latest {
            let deposit = match rollup.l1_to_l2_deposits.get(&index) {
                Some(deposit) => Deposit::from(deposit),
                None => {
                    warn!(
                        index = index,
                        "Midnight bridge missing deposit for on-chain index"
                    );
                    self.set_cursor(index.saturating_add(1));
                    continue;
                }
            };

            let event_id = deposit.event_id();
            if let Err(err) = self.submit_credit(&event_id, &deposit, Some(index)).await {
                warn!(
                    event_id = %event_id,
                    index = index,
                    nonce = deposit.nonce,
                    error = ?err,
                    "Midnight bridge failed to submit credit for on-chain deposit",
                );
                break;
            } else {
                self.set_cursor(index.saturating_add(1));
            }
        }

        Ok(())
    }

    async fn submit_credit(
        &mut self,
        event_id: &str,
        deposit: &Deposit,
        bridge_index: Option<u64>,
    ) -> Result<()> {
        let tx = self.build_mint_transaction(deposit)?;
        let tx_for_debug = tx.clone();
        let tx_hash = self.sequencer.accept_bridge_tx(tx).await.map_err(|err| {
            self.log_failed_submission(event_id, &tx_for_debug);
            anyhow!("Sequencer rejected Midnight bridge tx: {:?}", err)
        })?;

        let amount = Amount::from(deposit.amount);
        let recipient_address = deposit.recipient_address();
        let sender_hex = hex::encode(deposit.sender);
        let recipient_hex = hex::encode(deposit.recipient);
        let data_hash_hex = hex::encode(deposit.data_hash);

        info!(
            event_id = %event_id,
            tx_hash = ?tx_hash,
            amount = %amount,
            recipient = ?recipient_address,
            recipient_bytes = %recipient_hex,
            sender = %sender_hex,
            nonce = deposit.nonce,
            gas_limit = deposit.gas_limit,
            data_hash = %data_hash_hex,
            bridge_index,
            "Midnight bridge credited rollup funds",
        );

        Ok(())
    }

    fn build_mint_transaction(&mut self, deposit: &Deposit) -> Result<FullyBakedTx> {
        let runtime_call = RuntimeCall::<BridgeSpec>::Bank(BankCallMessage::Mint {
            coins: Coins {
                amount: Amount::from(deposit.amount),
                token_id: self.settings.token_id,
            },
            mint_to_address: deposit.recipient_address(),
        });

        let tx_details = TxDetails {
            max_priority_fee_bips: PriorityFeeBips::ZERO,
            max_fee: self.settings.max_fee,
            gas_limit: None,
            chain_id: config_chain_id(),
        };

        let unsigned = UnsignedTransaction::new_with_details(
            runtime_call,
            UniquenessData::Generation(self.next_generation),
            tx_details,
        );

        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("Midnight bridge generation overflow"))?;

        let signed = Transaction::<BridgeRuntime, BridgeSpec>::new_signed_tx(
            &self.settings.signing_key.private_key,
            &<BridgeRuntime as StfRuntime<BridgeSpec>>::CHAIN_HASH,
            unsigned,
        );

        let raw_tx = RawTx::new(
            borsh::to_vec(&signed).context("Failed to serialize Midnight bridge transaction")?,
        );

        Ok(BridgeAuthenticator::encode_with_standard_auth(raw_tx))
    }

    fn log_failed_submission(&self, event_id: &str, tx: &FullyBakedTx) {
        let tx_bytes = tx.data.len();
        let tx_base64 = BASE64_STANDARD.encode(&tx.data);
        let diagnostics = BridgeTxPayloadDiagnostics::new(tx);

        match (BridgeAuthenticator::decode_serialized_tx(tx), &diagnostics) {
            (Ok(call), Ok(diag)) => {
                info!(
                    event_id = %event_id,
                    tx_bytes,
                    raw_tx_bytes = diag.raw_tx_bytes,
                    raw_tx_hash = %diag.raw_tx_hash,
                    trailing_bytes = diag.trailing_bytes,
                    tx_base64 = %tx_base64,
                    ?call,
                    "Midnight bridge tx decoded locally despite sequencer error",
                );
            }
            (Ok(call), Err(diag_err)) => {
                info!(
                    event_id = %event_id,
                    tx_bytes,
                    tx_base64 = %tx_base64,
                    diagnostics_error = %diag_err,
                    ?call,
                    "Midnight bridge tx decoded locally despite sequencer error",
                );
            }
            (Err(decode_err), Ok(diag)) => {
                warn!(
                    event_id = %event_id,
                    tx_bytes,
                    raw_tx_bytes = diag.raw_tx_bytes,
                    raw_tx_hash = %diag.raw_tx_hash,
                    trailing_bytes = diag.trailing_bytes,
                    tx_base64 = %tx_base64,
                    error = ?decode_err,
                    "Midnight bridge tx failed to decode locally",
                );
            }
            (Err(decode_err), Err(diag_err)) => {
                warn!(
                    event_id = %event_id,
                    tx_bytes,
                    tx_base64 = %tx_base64,
                    diagnostics_error = %diag_err,
                    error = ?decode_err,
                    "Midnight bridge tx failed to decode locally",
                );
            }
        }
    }
}

struct BridgeTxPayloadDiagnostics {
    raw_tx_hash: TxHash,
    raw_tx_bytes: usize,
    trailing_bytes: usize,
}

impl BridgeTxPayloadDiagnostics {
    fn new(tx: &FullyBakedTx) -> Result<Self, String> {
        let mut cursor: &[u8] = &tx.data;
        let input: EvmAuthenticatorInput =
            EvmAuthenticatorInput::deserialize(&mut cursor).map_err(|err| err.to_string())?;
        let trailing_bytes = cursor.len();
        match input {
            EvmAuthenticatorInput::Standard(raw_tx) => {
                let raw_tx_hash = calculate_hash::<BridgeSpec>(&raw_tx.data);
                Ok(Self {
                    raw_tx_hash,
                    raw_tx_bytes: raw_tx.data.len(),
                    trailing_bytes,
                })
            }
            EvmAuthenticatorInput::StandardPreAuthenticated(raw_tx, original_hash) => Ok(Self {
                raw_tx_hash: original_hash,
                raw_tx_bytes: raw_tx.data.len(),
                trailing_bytes,
            }),
            EvmAuthenticatorInput::Evm(_) => Err(
                "Midnight bridge diagnostics do not support raw EVM-authenticated payloads"
                    .to_string(),
            ),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Deposit {
    #[serde(with = "hex_bytes")]
    pub(crate) sender: [u8; 32],
    #[serde(with = "hex_bytes")]
    pub(crate) recipient: [u8; 32],
    #[serde(with = "u128_string")]
    pub(crate) amount: u128,
    pub(crate) nonce: u64,
    pub(crate) gas_limit: u64,
    #[serde(with = "hex_bytes")]
    pub(crate) data_hash: [u8; 32],
}

impl Deposit {
    fn event_id(&self) -> String {
        format!(
            "midnight-deposit:{}:{}:{}",
            hex::encode(self.sender),
            self.nonce,
            hex::encode(self.data_hash)
        )
    }

    fn recipient_address(&self) -> <BridgeSpec as Spec>::Address {
        <BridgeSpec as Spec>::Address::from(CredentialId::from(self.recipient))
    }
}

impl From<&MidnightDeposit> for Deposit {
    fn from(value: &MidnightDeposit) -> Self {
        Self {
            sender: value.sender,
            recipient: value.recipient,
            amount: value.amount,
            nonce: value.nonce,
            gas_limit: value.gas_limit,
            data_hash: value.data_hash,
        }
    }
}

fn validate_contract_address(address: &str) -> Result<()> {
    if address.len() != 64 || !address.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("contract address must be 64 lowercase hex chars"));
    }
    Ok(())
}

async fn read_deposit_file(path: &Path) -> Result<Vec<Deposit>> {
    let contents = match fs::read_to_string(path).await {
        Ok(data) => data,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to read Midnight bridge events from {}",
                    path.display()
                )
            })
        }
    };

    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }

    serde_json::from_str(&contents).with_context(|| {
        format!(
            "Failed to parse Midnight bridge events from {}",
            path.display()
        )
    })
}

mod hex_bytes {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("0x{}", hex::encode(value)))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        let input = String::deserialize(deserializer)?;
        let trimmed = input.strip_prefix("0x").unwrap_or(&input);
        let decoded =
            hex::decode(trimmed).map_err(|err| serde::de::Error::custom(err.to_string()))?;
        if decoded.len() != 32 {
            return Err(serde::de::Error::custom(format!(
                "expected 32-byte hex string, got {} bytes",
                decoded.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&decoded);
        Ok(arr)
    }
}

mod u128_string {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        let input = String::deserialize(deserializer)?;
        input
            .parse::<u128>()
            .map_err(|err| serde::de::Error::custom(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::to_writer_pretty;
    use std::fs::File;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct RecordingSequencer {
        accepted: Mutex<Vec<FullyBakedTx>>,
        next_hash_byte: Mutex<u8>,
    }

    impl RecordingSequencer {
        async fn accepted(&self) -> Vec<FullyBakedTx> {
            self.accepted.lock().await.clone()
        }
    }

    #[async_trait]
    impl BridgeSequencer for RecordingSequencer {
        async fn accept_bridge_tx(
            &self,
            tx: FullyBakedTx,
        ) -> std::result::Result<TxHash, ErrorObject> {
            let mut accepted = self.accepted.lock().await;
            accepted.push(tx);

            let mut counter = self.next_hash_byte.lock().await;
            let mut hash = [0u8; 32];
            hash[0] = *counter;
            *counter = counter.wrapping_add(1);

            Ok(TxHash::new(hash))
        }

        async fn readiness_status(&self) -> std::result::Result<(), SequencerNotReadyDetails> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn bridge_processes_mock_events_once() {
        let temp_dir = tempdir().unwrap();
        let events_path = temp_dir.path().join("bridge_events.json");

        let deposits = vec![
            Deposit {
                sender: [0u8; 32],
                recipient: [1u8; 32],
                amount: 25u128,
                nonce: 7,
                gas_limit: 50_000,
                data_hash: [2u8; 32],
            },
            Deposit {
                sender: [3u8; 32],
                recipient: [4u8; 32],
                amount: 50u128,
                nonce: 8,
                gas_limit: 120_000,
                data_hash: [5u8; 32],
            },
        ];

        let mut file = File::create(&events_path).unwrap();
        to_writer_pretty(&mut file, &deposits).unwrap();

        let settings = RuntimeBridgeSettings {
            signing_key: PrivateKeyAndAddress::generate(),
            poll_interval: Duration::from_millis(10),
            token_id: config_gas_token_id(),
            max_fee: Amount::from(1_000_000u64),
        };

        let sequencer = Arc::new(RecordingSequencer::default());
        let deposit_source = DepositSource::Mock(MockDepositSource {
            events_path: events_path.clone(),
        });
        let mut bridge =
            MidnightBridge::new(
                Arc::clone(&sequencer),
                settings,
                deposit_source,
                None,
                None,
            )
            .unwrap();

        let snapshot = read_deposit_file(&events_path).await.unwrap();
        assert_eq!(snapshot.len(), deposits.len());

        bridge
            .process_mock_events(snapshot.clone(), &events_path)
            .await;
        bridge
            .process_mock_events(snapshot.clone(), &events_path)
            .await;

        let accepted = sequencer.accepted().await;
        assert_eq!(accepted.len(), deposits.len());

        for (tx, deposit) in accepted.iter().zip(snapshot.iter()) {
            let call = BridgeAuthenticator::decode_serialized_tx(tx).unwrap();
            let runtime_call = match call {
                EvmAuthenticatorInput::Standard(call) => call,
                _ => panic!("unexpected decoded call"),
            };

            match runtime_call {
                RuntimeCall::Bank(BankCallMessage::Mint {
                    coins,
                    mint_to_address,
                }) => {
                    assert_eq!(coins.amount, Amount::from(deposit.amount));
                    assert_eq!(coins.token_id, config_gas_token_id());
                    assert_eq!(mint_to_address, deposit.recipient_address());
                }
                _ => panic!("unexpected runtime call"),
            }
        }
    }

    #[tokio::test]
    async fn bridge_builds_tx_from_asset_signer() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let signer_path = manifest_dir.join("assets/midnight_bridge_signer.json");
        let events_path = manifest_dir.join("demo_data/midnight_bridge_events.json");

        let signing_key = PrivateKeyAndAddress::<BridgeSpec>::from_json_file(&signer_path, false)
            .expect("fixture signer loads");

        let settings = RuntimeBridgeSettings {
            signing_key,
            poll_interval: Duration::from_millis(10),
            token_id: config_gas_token_id(),
            max_fee: Amount::from(1_000_000u64),
        };

        let sequencer = Arc::new(RecordingSequencer::default());
        let deposit_source = DepositSource::Mock(MockDepositSource {
            events_path: events_path.clone(),
        });
        let mut bridge = MidnightBridge::new(
            sequencer,
            settings,
            deposit_source,
            None,
            None,
        )
        .unwrap();

        let deposit = read_deposit_file(&events_path)
            .await
            .expect("fixture events parse")
            .into_iter()
            .next()
            .expect("fixture event exists");

        let tx = bridge
            .build_mint_transaction(&deposit)
            .expect("tx build succeeds");
        let call = BridgeAuthenticator::decode_serialized_tx(&tx).expect("tx decodes");
        let runtime_call = match call {
            EvmAuthenticatorInput::Standard(call) => call,
            _ => panic!("unexpected decoded call"),
        };

        match runtime_call {
            RuntimeCall::Bank(BankCallMessage::Mint {
                coins,
                mint_to_address,
            }) => {
                assert_eq!(coins.amount, Amount::from(deposit.amount));
                assert_eq!(coins.token_id, config_gas_token_id());
                assert_eq!(mint_to_address, deposit.recipient_address());
            }
            _ => panic!("unexpected runtime call"),
        }
    }

    #[tokio::test]
    async fn bridge_computes_hash_for_fixture_tx() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let signer_path = manifest_dir.join("assets/midnight_bridge_signer.json");
        let events_path = manifest_dir.join("demo_data/midnight_bridge_events.json");

        let signing_key = PrivateKeyAndAddress::<BridgeSpec>::from_json_file(&signer_path, false)
            .expect("fixture signer loads");

        let settings = RuntimeBridgeSettings {
            signing_key,
            poll_interval: Duration::from_millis(10),
            token_id: config_gas_token_id(),
            max_fee: Amount::from(1_000_000u64),
        };

        let sequencer = Arc::new(RecordingSequencer::default());
        let deposit_source = DepositSource::Mock(MockDepositSource {
            events_path: events_path.clone(),
        });
        let mut bridge = MidnightBridge::new(
            sequencer,
            settings,
            deposit_source,
            None,
            None,
        )
        .unwrap();

        let deposit = read_deposit_file(&events_path)
            .await
            .expect("fixture events parse")
            .into_iter()
            .next()
            .expect("fixture event exists");

        let tx = bridge
            .build_mint_transaction(&deposit)
            .expect("tx build succeeds");
        let tx_base64 = BASE64_STANDARD.encode(&tx.data);
        assert_eq!(
            tx_base64,
            "AdoAAAAA+Y16HHlbDOH8dRML02ZZG6Mj/MgjDwK4W5s4TU5N5Dj898eel61gGO9ekou/cA59OVGsaRaZiFp+UqSokEOlAPitJDeieeHIkywHNYyR3E/jSGSpjGwl8pjioBmcFQn/AAOgJSYAAAAAAAAAAAAAAAAAmT78vI7I+oTDqyIWqottPIY1MAtjUhWKVzWRwSjGNFkA/ty6mHZUMhABI0VniavN7wARIjNEVWZ3iJmquwEAAAAAAAAAAAAAAAAAAAAAQEIPAAAAAAAAAAAAAAAAAADhEAAAAAAAAA==",
            "fixture payload matches logged base64",
        );

        let diagnostics =
            BridgeTxPayloadDiagnostics::new(&tx).expect("diagnostics decode succeeds");
        assert_eq!(tx.data[0], 1, "variant index should indicate Standard auth");
        let raw_len = u32::from_le_bytes(tx.data[1..5].try_into().unwrap()) as usize;
        assert_eq!(raw_len, diagnostics.raw_tx_bytes);
        assert_eq!(raw_len, tx.data.len() - 5);
        let hash = BridgeAuthenticator::compute_tx_hash(&tx)
            .expect("hash computation succeeds once authenticator bug is fixed");
        assert_eq!(hash, diagnostics.raw_tx_hash);
    }
}

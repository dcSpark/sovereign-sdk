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
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sov_bank::{config_gas_token_id, CallMessage as BankCallMessage, Coins, TokenId};
use sov_cli::wallet_state::PrivateKeyAndAddress;
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
use sov_modules_api::{Amount, RawTx, Spec};
use sov_rollup_interface::TxHash;
use sov_sequencer::{Sequencer, SequencerNotReadyDetails};
use tokio::fs;
use tokio::task::JoinHandle;
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::MockRollupSpec;
use sov_modules_stf_blueprint::Runtime as StfRuntime;

type BridgeSpec = MockRollupSpec<Native>;
type BridgeRuntime = Runtime<BridgeSpec>;
type BridgeAuthenticator = <BridgeRuntime as StfRuntime<BridgeSpec>>::Auth;

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
pub(crate) fn spawn_midnight_bridge<Seq>(
    sequencer: Arc<Seq>,
    extension: &SeqConfigExtension,
) -> Result<Option<JoinHandle<anyhow::Result<()>>>>
where
    Seq: BridgeSequencer,
{
    let Some(settings) = load_runtime_settings(extension)? else {
        debug!("Midnight bridge disabled");
        return Ok(None);
    };

    info!(
        poll_interval_ms = settings.poll_interval.as_millis() as u64,
        path = %settings.events_path.display(),
        "Starting Midnight bridge background task",
    );

    let bridge = MidnightBridge::new(sequencer, settings);
    Ok(Some(tokio::spawn(async move { bridge.run().await })))
}

fn load_runtime_settings(extension: &SeqConfigExtension) -> Result<Option<RuntimeBridgeSettings>> {
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

    let events_path = raw.mock_events_path.clone().ok_or_else(|| {
        anyhow!("mock_events_path must be provided when enabling the Midnight bridge")
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

    Ok(Some(RuntimeBridgeSettings {
        signing_key,
        poll_interval,
        events_path,
        token_id,
        max_fee,
    }))
}

struct RuntimeBridgeSettings {
    signing_key: PrivateKeyAndAddress<BridgeSpec>,
    poll_interval: Duration,
    events_path: PathBuf,
    token_id: TokenId,
    max_fee: Amount,
}

struct MidnightBridge<Seq> {
    sequencer: Arc<Seq>,
    settings: RuntimeBridgeSettings,
    processed_event_ids: HashSet<String>,
    next_generation: u64,
    idle_notice_sent: bool,
}

impl<Seq> MidnightBridge<Seq>
where
    Seq: BridgeSequencer,
{
    fn new(sequencer: Arc<Seq>, settings: RuntimeBridgeSettings) -> Self {
        Self {
            sequencer,
            settings,
            processed_event_ids: HashSet::new(),
            next_generation: 0,
            idle_notice_sent: false,
        }
    }

    async fn run(mut self) -> Result<()> {
        let mut ticker = interval(self.settings.poll_interval);
        loop {
            ticker.tick().await;
            match self.fetch_events().await {
                Ok(events) => self.process_events(events).await,
                Err(err) => warn!(error = ?err, "Midnight bridge failed to fetch events"),
            }
        }
    }

    async fn fetch_events(
        &self,
    ) -> Result<Vec<MidnightBridgeEvent<<BridgeSpec as Spec>::Address>>> {
        read_event_file(&self.settings.events_path).await
    }

    async fn process_events(
        &mut self,
        events: Vec<MidnightBridgeEvent<<BridgeSpec as Spec>::Address>>,
    ) {
        if events.is_empty() {
            if !self.idle_notice_sent {
                info!(
                    path = %self.settings.events_path.display(),
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

        for event in events {
            if self.processed_event_ids.contains(&event.id) {
                continue;
            }

            match self.submit_credit(&event).await {
                Ok(()) => {
                    self.processed_event_ids.insert(event.id.clone());
                }
                Err(err) => {
                    warn!(event_id = %event.id, error = ?err, "Midnight bridge failed to submit credit");
                }
            }
        }
    }

    async fn submit_credit(
        &mut self,
        event: &MidnightBridgeEvent<<BridgeSpec as Spec>::Address>,
    ) -> Result<()> {
        let tx = self.build_mint_transaction(event)?;
        let tx_for_debug = tx.clone();
        let tx_hash = self.sequencer.accept_bridge_tx(tx).await.map_err(|err| {
            self.log_failed_submission(event, &tx_for_debug);
            anyhow!("Sequencer rejected Midnight bridge tx: {:?}", err)
        })?;

        info!(
            event_id = %event.id,
            tx_hash = ?tx_hash,
            amount = ?event.amount,
            recipient = ?event.recipient,
            "Midnight bridge credited rollup funds",
        );

        Ok(())
    }

    fn build_mint_transaction(
        &mut self,
        event: &MidnightBridgeEvent<<BridgeSpec as Spec>::Address>,
    ) -> Result<FullyBakedTx> {
        let runtime_call = RuntimeCall::<BridgeSpec>::Bank(BankCallMessage::Mint {
            coins: Coins {
                amount: event.amount,
                token_id: self.settings.token_id,
            },
            mint_to_address: event.recipient.clone(),
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

    fn log_failed_submission(
        &self,
        event: &MidnightBridgeEvent<<BridgeSpec as Spec>::Address>,
        tx: &FullyBakedTx,
    ) {
        let tx_bytes = tx.data.len();
        let tx_base64 = BASE64_STANDARD.encode(&tx.data);
        let diagnostics = BridgeTxPayloadDiagnostics::new(tx);

        match (BridgeAuthenticator::decode_serialized_tx(tx), &diagnostics) {
            (Ok(call), Ok(diag)) => {
                info!(
                    event_id = %event.id,
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
                    event_id = %event.id,
                    tx_bytes,
                    tx_base64 = %tx_base64,
                    diagnostics_error = %diag_err,
                    ?call,
                    "Midnight bridge tx decoded locally despite sequencer error",
                );
            }
            (Err(decode_err), Ok(diag)) => {
                warn!(
                    event_id = %event.id,
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
                    event_id = %event.id,
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
#[serde(bound(deserialize = "Address: DeserializeOwned"))]
struct MidnightBridgeEvent<Address> {
    /// Unique identifier for the event (used for idempotency).
    pub id: String,
    /// Amount of tokens to mint (human-readable number in the JSON file).
    pub amount: Amount,
    /// Rollup recipient address.
    pub recipient: Address,
}

async fn read_event_file<Address>(path: &Path) -> Result<Vec<MidnightBridgeEvent<Address>>>
where
    Address: DeserializeOwned,
{
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
    }

    #[tokio::test]
    async fn bridge_processes_mock_events_once() {
        let temp_dir = tempdir().unwrap();
        let events_path = temp_dir.path().join("bridge_events.json");

        let recipient = PrivateKeyAndAddress::<BridgeSpec>::generate().address;
        let events = vec![
            MidnightBridgeEvent {
                id: "evt-1".to_string(),
                amount: Amount::from(25u64),
                recipient: recipient.clone(),
            },
            MidnightBridgeEvent {
                id: "evt-2".to_string(),
                amount: Amount::from(50u64),
                recipient: recipient.clone(),
            },
        ];

        let mut file = File::create(&events_path).unwrap();
        to_writer_pretty(&mut file, &events).unwrap();

        let settings = RuntimeBridgeSettings {
            signing_key: PrivateKeyAndAddress::generate(),
            poll_interval: Duration::from_millis(10),
            events_path: events_path.clone(),
            token_id: config_gas_token_id(),
            max_fee: Amount::from(1_000_000u64),
        };

        let sequencer = Arc::new(RecordingSequencer::default());
        let mut bridge = MidnightBridge::new(Arc::clone(&sequencer), settings);

        let snapshot = bridge.fetch_events().await.unwrap();
        assert_eq!(snapshot.len(), events.len());

        bridge.process_events(snapshot.clone()).await;
        bridge.process_events(snapshot).await;

        let accepted = sequencer.accepted().await;
        assert_eq!(accepted.len(), events.len());

        for (tx, event) in accepted.iter().zip(events.iter()) {
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
                    assert_eq!(coins.amount, event.amount);
                    assert_eq!(coins.token_id, config_gas_token_id());
                    assert_eq!(&mint_to_address, &event.recipient);
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
        let events: Vec<MidnightBridgeEvent<<BridgeSpec as Spec>::Address>> =
            read_event_file(&events_path)
                .await
                .expect("fixture events parse");
        let event = events.first().expect("fixture event exists").clone();

        let settings = RuntimeBridgeSettings {
            signing_key,
            poll_interval: Duration::from_millis(10),
            events_path: events_path.clone(),
            token_id: config_gas_token_id(),
            max_fee: Amount::from(1_000_000u64),
        };

        let sequencer = Arc::new(RecordingSequencer::default());
        let mut bridge = MidnightBridge::new(sequencer, settings);

        let tx = bridge
            .build_mint_transaction(&event)
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
                assert_eq!(coins.amount, event.amount);
                assert_eq!(coins.token_id, config_gas_token_id());
                assert_eq!(mint_to_address, event.recipient);
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
        let events: Vec<MidnightBridgeEvent<<BridgeSpec as Spec>::Address>> =
            read_event_file(&events_path)
                .await
                .expect("fixture events parse");
        let event = events.first().expect("fixture event exists").clone();

        let settings = RuntimeBridgeSettings {
            signing_key,
            poll_interval: Duration::from_millis(10),
            events_path: events_path.clone(),
            token_id: config_gas_token_id(),
            max_fee: Amount::from(1_000_000u64),
        };

        let sequencer = Arc::new(RecordingSequencer::default());
        let mut bridge = MidnightBridge::new(sequencer, settings);

        let tx = bridge
            .build_mint_transaction(&event)
            .expect("tx build succeeds");
        let tx_base64 = BASE64_STANDARD.encode(&tx.data);
        assert_eq!(
            tx_base64,
            "AdoAAAAA8M6hxV+IgZZO9tpQ1l7oTlRTHDjIum5K74om3OddGFOJ1YMxiSXsvyQXdGnes0SWbRR2Tlr+bdsrHkjLuhfaBvitJDeieeHIkywHNYyR3E/jSGSpjGwl8pjioBmcFQn/AAOgJSYAAAAAAAAAAAAAAAAAmT78vI7I+oTDqyIWqottPIY1MAtjUhWKVzWRwSjGNFkAe3WL8udnD6+va/ABXOD/WqgCMG/H4/RXYoU//AEAAAAAAAAAAAAAAAAAAAAAQEIPAAAAAAAAAAAAAAAAAADhEAAAAAAAAA==",
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

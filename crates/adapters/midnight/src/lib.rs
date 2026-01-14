use anyhow::{anyhow, Context, Result};
use base_crypto::fab::{AlignedValue, ValueAtom};
use hex::FromHex;
use midnight_onchain_state::state::{ChargedState, ContractMaintenanceAuthority, ContractState};
use midnight_serialize::{tagged_deserialize, Deserializable};
use midnight_storage::arena::{set_allow_non_normal_form_deserialization, Sp};
use midnight_storage::db::InMemoryDB;
use midnight_storage::storage::HashMap as StorageHashMap;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::ops::Deref;
use std::sync::Once;

const LEGACY_CONTRACT_STATE_TAG_V4: &[u8] = b"midnight:contract-state[v4]:";

const CONTRACT_STATE_QUERY: &str = r#"
query CONTRACT_STATE_QUERY($address: HexEncoded!, $offset: ContractActionOffset) {
contractAction(address: $address, offset: $offset) {
state
}
}
"#;

static SET_NORMAL_FORM_FLAG: Once = Once::new();

/// Client wrapper for querying the Midnight GraphQL indexer.
pub struct MidnightIndexerClient {
    client: Client,
    endpoint: String,
    contract_address: String,
}

impl MidnightIndexerClient {
    /// Builds a client for the Midnight indexer GraphQL endpoint.
    pub fn new(client: Client, endpoint: String, contract_address: String) -> Self {
        SET_NORMAL_FORM_FLAG.call_once(|| set_allow_non_normal_form_deserialization(true));
        Self {
            client,
            endpoint,
            contract_address,
        }
    }

    /// Returns the configured GraphQL endpoint URL.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the Midnight contract address the client tracks.
    pub fn contract_address(&self) -> &str {
        &self.contract_address
    }

    /// Fetches and parses the latest Midnight bridge contract state snapshot.
    pub async fn snapshot(&self) -> Result<BridgeContractSnapshot> {
        let state_bytes = self.fetch_contract_state().await?;
        let contract_state = deserialize_contract_state(&state_bytes)?;
        analyze_bridge_state(&contract_state)
    }

    async fn fetch_contract_state(&self) -> Result<Vec<u8>> {
        let body = json!({
            "query": CONTRACT_STATE_QUERY,
            "variables": {
                "address": self.contract_address,
                "offset": null,
            }
        });

        let resp = self
            .client
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .context("Midnight indexer GraphQL request failed")?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "Midnight indexer request failed with status {}",
                resp.status()
            ));
        }

        let payload: GraphQlResponse<ContractActionWrapper> = resp.json().await?;
        if let Some(errors) = payload.errors {
            let joined = errors
                .into_iter()
                .map(|e| e.message)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow!("Midnight indexer GraphQL error: {}", joined));
        }

        let state_hex = payload
            .data
            .and_then(|wrapper| wrapper.contract_action)
            .map(|action| action.state)
            .ok_or_else(|| anyhow!("Midnight indexer returned empty contract state"))?;

        let bytes = Vec::from_hex(state_hex)
            .map_err(|err| anyhow!("failed to decode contract state hex: {}", err))?;
        Ok(bytes)
    }
}

/// Raw deposit record extracted from the Midnight bridge contract state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidnightDeposit {
    pub sender: [u8; 32],
    pub recipient: [u8; 32],
    pub amount: u128,
    pub nonce: u64,
    pub gas_limit: u64,
    pub data_hash: [u8; 32],
}

/// Snapshot of the Midnight bridge contract state tailored for rollup ingestion.
#[derive(Debug, Clone)]
pub struct BridgeContractSnapshot {
    pub next_cross_domain_message_index: u64,
    pub deposits: BTreeMap<u64, MidnightDeposit>,
}

fn analyze_bridge_state(state: &ContractState<InMemoryDB>) -> Result<BridgeContractSnapshot> {
    let root = state.data.get_ref();
    let deposit_node = locate_deposit_map(root)
        .ok_or_else(|| anyhow!("failed to locate deposit map in contract state"))?;
    let deposits = extract_deposits(deposit_node.as_ref())?;
    let next_cross = deposits
        .keys()
        .next_back()
        .map(|last| last.saturating_add(1))
        .unwrap_or(0);
    Ok(BridgeContractSnapshot {
        next_cross_domain_message_index: next_cross,
        deposits,
    })
}

fn deserialize_contract_state(bytes: &[u8]) -> Result<ContractState<InMemoryDB>> {
    match tagged_deserialize(bytes) {
        Ok(state) => Ok(state),
        Err(primary_err) => match deserialize_contract_state_v4(bytes) {
            Ok(Some(legacy)) => Ok(legacy),
            Ok(None) => Err(anyhow!(
                "failed to deserialize ContractState: {}",
                primary_err
            )),
            Err(legacy_err) => Err(anyhow!(
                "failed to deserialize ContractState: {}; legacy decode error: {}",
                primary_err,
                legacy_err
            )),
        },
    }
}

fn deserialize_contract_state_v4(bytes: &[u8]) -> Result<Option<ContractState<InMemoryDB>>> {
    if !bytes.starts_with(LEGACY_CONTRACT_STATE_TAG_V4) {
        return Ok(None);
    }

    let mut reader = Cursor::new(&bytes[LEGACY_CONTRACT_STATE_TAG_V4.len()..]);
    let legacy_value = <StateValue as Deserializable>::deserialize(&mut reader, 0)
        .map_err(|err| anyhow!("legacy contract-state data decode failed: {}", err))?;
    let data = ChargedState::new(legacy_value);

    Ok(Some(ContractState {
        data,
        operations: StorageHashMap::new(),
        maintenance_authority: ContractMaintenanceAuthority::new(),
        balance: StorageHashMap::new(),
    }))
}

fn locate_deposit_map<'a>(root: &'a StateValue) -> Option<StateNode<'a>> {
    locate_with_policy(root, false).or_else(|| locate_with_policy(root, true))
}

fn locate_with_policy<'a>(root: &'a StateValue, allow_empty: bool) -> Option<StateNode<'a>> {
    fn helper<'a>(node: StateNode<'a>, allow_empty: bool) -> Option<StateNode<'a>> {
        if is_deposit_map(node.as_ref(), allow_empty) {
            return Some(node);
        }
        match node.as_ref() {
            StateValue::Map(map) => {
                for entry in map.iter() {
                    let (_, child) = entry.deref();
                    if let Some(found) = helper(StateNode::Owned(child.clone()), allow_empty) {
                        return Some(found);
                    }
                }
            }
            StateValue::Array(arr) => {
                for elem in arr.iter() {
                    if let Some(found) = helper(StateNode::Owned(elem.clone()), allow_empty) {
                        return Some(found);
                    }
                }
            }
            _ => {}
        }
        None
    }

    helper(StateNode::Borrowed(root), allow_empty)
}

fn is_deposit_map(value: &StateValue, allow_empty: bool) -> bool {
    let StateValue::Map(map) = value else {
        return false;
    };
    if map.size() == 0 {
        return allow_empty;
    }
    let mut has_deposit = false;
    for entry in map.iter() {
        let (key_sp, value_sp) = entry.deref();
        if decode_u64(key_sp.deref()).is_err() {
            return false;
        }
        if matches!(value_sp.deref(), StateValue::Null) {
            continue;
        }
        match decode_deposit(value_sp.deref()) {
            Ok(Some(_)) => {
                has_deposit = true;
            }
            _ => return false,
        }
    }
    has_deposit
}

fn extract_deposits(value: &StateValue) -> Result<BTreeMap<u64, MidnightDeposit>> {
    match value {
        StateValue::Map(map) => {
            let mut result = BTreeMap::new();
            for pair in map.iter() {
                let (key_sp, value_sp) = pair.deref();
                let idx = decode_u64(key_sp.deref())?;
                if let Some(deposit) = decode_deposit(value_sp.deref())? {
                    result.insert(idx, deposit);
                }
            }
            Ok(result)
        }
        _ => Err(anyhow!("expected deposits map")),
    }
}

fn decode_deposit(value: &StateValue) -> Result<Option<MidnightDeposit>> {
    match value {
        StateValue::Array(fields) if fields.len() == 6 => {
            let mut elems = fields.iter();
            let sender = extract_bytes32(
                elems
                    .next()
                    .ok_or_else(|| anyhow!("deposit missing sender"))?
                    .deref(),
            )?;
            let recipient = extract_bytes32(
                elems
                    .next()
                    .ok_or_else(|| anyhow!("deposit missing recipient"))?
                    .deref(),
            )?;
            let amount = extract_cell_u128(
                elems
                    .next()
                    .ok_or_else(|| anyhow!("deposit missing amount"))?
                    .deref(),
            )?;
            let nonce = extract_cell_u64(
                elems
                    .next()
                    .ok_or_else(|| anyhow!("deposit missing nonce"))?
                    .deref(),
            )?;
            let gas_limit = extract_cell_u64(
                elems
                    .next()
                    .ok_or_else(|| anyhow!("deposit missing gas limit"))?
                    .deref(),
            )?;
            let data_hash = extract_bytes32(
                elems
                    .next()
                    .ok_or_else(|| anyhow!("deposit missing data hash"))?
                    .deref(),
            )?;
            Ok(Some(MidnightDeposit {
                sender,
                recipient,
                amount,
                nonce,
                gas_limit,
                data_hash,
            }))
        }
        StateValue::Cell(cell) => decode_deposit_cell(cell.deref()),
        StateValue::Null => Ok(None),
        _ => Ok(None),
    }
}

fn decode_deposit_cell(cell: &AlignedValue) -> Result<Option<MidnightDeposit>> {
    const EXPECTED_FIELDS: usize = 6;
    if cell.value.0.len() != EXPECTED_FIELDS {
        return Ok(None);
    }
    let mut atoms = cell.value.0.iter();
    let sender = decode_bytes_atom::<32>(atoms.next().unwrap(), "sender")?;
    let recipient = decode_bytes_atom::<32>(atoms.next().unwrap(), "recipient")?;
    let amount = decode_u128_atom(atoms.next().unwrap(), "amount")?;
    let nonce = decode_u64_atom(atoms.next().unwrap(), "nonce")?;
    let gas_limit = decode_u64_atom(atoms.next().unwrap(), "gas limit")?;
    let data_hash = decode_bytes_atom::<32>(atoms.next().unwrap(), "data hash")?;
    Ok(Some(MidnightDeposit {
        sender,
        recipient,
        amount,
        nonce,
        gas_limit,
        data_hash,
    }))
}

fn decode_bytes_atom<const N: usize>(atom: &ValueAtom, label: &str) -> Result<[u8; N]> {
    if atom.0.len() > N {
        return Err(anyhow!("{} field exceeded {} bytes", label, N));
    }
    let mut out = [0u8; N];
    out[..atom.0.len()].copy_from_slice(&atom.0);
    Ok(out)
}

fn decode_u64_atom(atom: &ValueAtom, label: &str) -> Result<u64> {
    u64::try_from(atom).map_err(|err| anyhow!("failed to decode {}: {}", label, err))
}

fn decode_u128_atom(atom: &ValueAtom, label: &str) -> Result<u128> {
    u128::try_from(atom).map_err(|err| anyhow!("failed to decode {}: {}", label, err))
}

fn extract_bytes32(value: &StateValue) -> Result<[u8; 32]> {
    let aligned = expect_cell(value)?;
    let slice = aligned.as_slice();
    let bytes =
        Vec::<u8>::try_from(&*slice).map_err(|_| anyhow!("failed to decode bytes field"))?;
    if bytes.len() > 32 {
        return Err(anyhow!("bytes32 field exceeded length"));
    }
    let mut out = [0u8; 32];
    out[..bytes.len()].copy_from_slice(&bytes);
    Ok(out)
}

fn extract_cell_u64(value: &StateValue) -> Result<u64> {
    let aligned = expect_cell(value)?;
    decode_u64(aligned)
}

fn extract_cell_u128(value: &StateValue) -> Result<u128> {
    let aligned = expect_cell(value)?;
    decode_u128(aligned)
}

fn expect_cell<'a>(value: &'a StateValue) -> Result<&'a AlignedValue> {
    if let StateValue::Cell(cell) = value {
        Ok(cell)
    } else {
        Err(anyhow!("expected cell state value"))
    }
}

fn decode_u64(aligned: &AlignedValue) -> Result<u64> {
    let slice = aligned.as_slice();
    u64::try_from(&*slice).map_err(|_| anyhow!("failed to decode u64"))
}

fn decode_u128(aligned: &AlignedValue) -> Result<u128> {
    let slice = aligned.as_slice();
    u128::try_from(&*slice).map_err(|_| anyhow!("failed to decode u128"))
}

enum StateNode<'a> {
    Borrowed(&'a StateValue),
    Owned(Sp<StateValue>),
}

impl<'a> StateNode<'a> {
    fn as_ref(&self) -> &StateValue {
        match self {
            StateNode::Borrowed(value) => value,
            StateNode::Owned(sp) => sp.deref(),
        }
    }
}

#[derive(Deserialize)]
struct GraphQlResponse<T> {
    data: Option<T>,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Deserialize)]
struct ContractActionWrapper {
    #[serde(rename = "contractAction")]
    contract_action: Option<ContractActionState>,
}

#[derive(Deserialize)]
struct ContractActionState {
    state: String,
}

type StateValue = midnight_onchain_state::state::StateValue<InMemoryDB>;

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use std::env;
    use std::time::Duration;

    const ENDPOINT_ENV: &str = "MIDNIGHT_INDEXER_ENDPOINT";
    const CONTRACT_ENV: &str = "MIDNIGHT_CONTRACT_ADDRESS";

    #[tokio::test(flavor = "multi_thread")]
    async fn fetches_real_snapshot_when_configured() -> Result<()> {
        let (endpoint, contract) = match read_real_config() {
            Some(values) => values,
            None => {
                eprintln!(
                    "Skipping real Midnight indexer test. Set {} and {} to run it.",
                    ENDPOINT_ENV, CONTRACT_ENV,
                );
                return Ok(());
            }
        };

        let http = Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .context("failed to build Midnight indexer HTTP client for test")?;

        let client = MidnightIndexerClient::new(http, endpoint, contract);
        let snapshot = client
            .snapshot()
            .await
            .context("failed to fetch Midnight bridge snapshot")?;

        assert!(snapshot.next_cross_domain_message_index >= snapshot.deposits.len() as u64);
        Ok(())
    }

    fn read_real_config() -> Option<(String, String)> {
        let endpoint = env::var(ENDPOINT_ENV).ok()?.trim().to_owned();
        let contract = env::var(CONTRACT_ENV).ok()?.trim().to_owned();
        if endpoint.is_empty() || contract.is_empty() {
            return None;
        }
        Some((endpoint, contract))
    }
}

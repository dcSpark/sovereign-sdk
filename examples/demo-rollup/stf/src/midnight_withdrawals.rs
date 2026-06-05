//! Minimal Midnight L2 -> L1 withdrawal prototype.

use std::array;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::LazyLock;

use anyhow::{bail, ensure, Context, Result};
use borsh::{BorshDeserialize, BorshSerialize};
use hex::{decode, encode};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sov_bank::{config_gas_token_id, Bank, Coins, TokenId};
use sov_midnight_adapter::protocol_types::{
    hash_merkle_node, hash_withdraw_leaf, hash_withdraw_message, WithdrawMessage, ZERO_BYTES32,
};
use sov_modules_api::macros::{serialize, UniversalWallet};
use sov_modules_api::{
    Amount, Context as ModuleContext, DaSpec, EventEmitter, GenesisState, Module, ModuleId,
    ModuleInfo, ModuleRestApi, SafeString, Spec, StateMap, StateValue, TxState,
};
use strum::{EnumDiscriminants, EnumIs, VariantArray};

#[cfg(feature = "native")]
use sov_modules_api::prelude::axum::{self, extract::Query, routing::get, Router};
#[cfg(feature = "native")]
use sov_modules_api::prelude::UnwrapInfallible;
#[cfg(feature = "native")]
use sov_modules_api::rest::utils::{errors, ApiResult, Path};
#[cfg(feature = "native")]
use sov_modules_api::rest::{ApiState, HasCustomRestApi};
#[cfg(feature = "native")]
use sov_modules_api::ApiStateAccessor;

/// Midnight L1 identifier used by the prototype bridge.
pub type MidnightAddress = SafeString;

/// Genesis configuration for the Midnight withdrawal prototype.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct MidnightWithdrawalsConfig {
    /// Enables or disables the withdrawal call entirely.
    pub enabled: bool,
}

/// Basic L2 -> L1 withdrawal queue storing burn records for later proof generation.
#[derive(Clone, ModuleInfo, ModuleRestApi)]
pub struct MidnightWithdrawals<S: Spec> {
    /// Module identifier assigned by the runtime.
    #[id]
    pub id: ModuleId,
    /// Feature flag controlled at genesis.
    #[state]
    pub enabled: StateValue<bool>,
    /// Number of messages appended to the withdrawal queue.
    #[state]
    pub message_count: StateValue<u64>,
    /// Current Merkle root of the withdrawal queue.
    #[state]
    pub withdraw_root: StateValue<[u8; 32]>,
    /// Cached branch hash at level 0.
    #[state]
    pub branch_0: StateValue<[u8; 32]>,
    /// Cached branch hash at level 1.
    #[state]
    pub branch_1: StateValue<[u8; 32]>,
    /// Cached branch hash at level 2.
    #[state]
    pub branch_2: StateValue<[u8; 32]>,
    /// Cached branch hash at level 3.
    #[state]
    pub branch_3: StateValue<[u8; 32]>,
    /// Cached branch hash at level 4.
    #[state]
    pub branch_4: StateValue<[u8; 32]>,
    /// Cached branch hash at level 5.
    #[state]
    pub branch_5: StateValue<[u8; 32]>,
    /// Cached branch hash at level 6.
    #[state]
    pub branch_6: StateValue<[u8; 32]>,
    /// Cached branch hash at level 7.
    #[state]
    pub branch_7: StateValue<[u8; 32]>,
    /// Cached branch hash at level 8.
    #[state]
    pub branch_8: StateValue<[u8; 32]>,
    /// Cached branch hash at level 9.
    #[state]
    pub branch_9: StateValue<[u8; 32]>,
    /// Cached branch hash at level 10.
    #[state]
    pub branch_10: StateValue<[u8; 32]>,
    /// Cached branch hash at level 11.
    #[state]
    pub branch_11: StateValue<[u8; 32]>,
    /// Cached branch hash at level 12.
    #[state]
    pub branch_12: StateValue<[u8; 32]>,
    /// Cached branch hash at level 13.
    #[state]
    pub branch_13: StateValue<[u8; 32]>,
    /// Cached branch hash at level 14.
    #[state]
    pub branch_14: StateValue<[u8; 32]>,
    /// Cached branch hash at level 15.
    #[state]
    pub branch_15: StateValue<[u8; 32]>,
    /// Persisted map of all withdrawal records by nonce.
    #[state]
    pub withdrawals: StateMap<u64, StoredWithdrawal<S>>,
    /// Reference to the bank module, used to burn the canonical gas token (NIGHT).
    #[module]
    pub bank: Bank<S>,
}

/// Internal record mirroring a queued withdrawal for REST exposures and proofs.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct StoredWithdrawal<S: Spec> {
    nonce: u64,
    midnight_address: MidnightAddress,
    recipient_bytes: [u8; 32],
    sender_bytes: [u8; 32],
    amount: Amount,
    token_id: TokenId,
    l2_sender: S::Address,
    gas_limit: Option<u64>,
    message_hash: [u8; 32],
    leaf_hash: [u8; 32],
}

#[derive(Debug, Clone)]
struct WithdrawalProofBundle<S: Spec> {
    record: StoredWithdrawal<S>,
    siblings: [[u8; 32]; TREE_DEPTH],
    index_bits_le: [bool; TREE_DEPTH],
    leaf_count: u64,
    withdraw_root: [u8; 32],
}

/// Rust-side representation of Compact's `WithdrawProof16` struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct L1WithdrawProof16Binary {
    /// Finalized batch index that attested the withdraw root.
    pub batch_index: u64,
    /// Nonce assigned by the withdrawal queue.
    pub nonce: u64,
    /// Merkle index bits (little-endian, level 0 first).
    pub index_bits_le: [bool; TREE_DEPTH],
    /// Merkle sibling nodes ordered from leaves to root.
    pub siblings: [[u8; 32]; TREE_DEPTH],
}

impl L1WithdrawProof16Binary {
    fn new(
        batch_index: u64,
        nonce: u64,
        index_bits_le: [bool; TREE_DEPTH],
        siblings: [[u8; 32]; TREE_DEPTH],
    ) -> Self {
        Self {
            batch_index,
            nonce,
            index_bits_le,
            siblings,
        }
    }

    #[cfg(feature = "native")]
    fn into_response(self) -> L1WithdrawProof16Response {
        L1WithdrawProof16Response {
            batch_index: self.batch_index,
            nonce: self.nonce,
            index_bits_le: self.index_bits_le,
            sibling_hashes_hex: array::from_fn(|idx| encode(self.siblings[idx])),
        }
    }
}

const TREE_DEPTH: usize = 16;
const MAX_LEAVES: u64 = 1u64 << TREE_DEPTH;

static ZERO_HASHES: LazyLock<[[u8; 32]; TREE_DEPTH]> = LazyLock::new(|| {
    let mut hashes = [[0u8; 32]; TREE_DEPTH];
    hashes[0] = hash_merkle_node(&ZERO_BYTES32, &ZERO_BYTES32);
    for level in 1..TREE_DEPTH {
        hashes[level] = hash_merkle_node(&hashes[level - 1], &hashes[level - 1]);
    }
    hashes
});

fn zero_hash(level: usize) -> [u8; 32] {
    ZERO_HASHES[level]
}

fn zero_sibling(level: usize) -> [u8; 32] {
    if level == 0 {
        ZERO_BYTES32
    } else {
        zero_hash(level - 1)
    }
}

fn index_bits_le(nonce: u64) -> [bool; TREE_DEPTH] {
    let mut bits = [false; TREE_DEPTH];
    for level in 0..TREE_DEPTH {
        bits[level] = ((nonce >> level) & 1) == 1;
    }
    bits
}

struct MerkleArtifacts {
    siblings: [[u8; 32]; TREE_DEPTH],
    index_bits_le: [bool; TREE_DEPTH],
    root: [u8; 32],
}

fn compute_merkle_artifacts<F>(
    nonce: u64,
    leaf_count: u64,
    fetch_leaf: &mut F,
) -> Result<MerkleArtifacts>
where
    F: FnMut(u64) -> Result<[u8; 32]>,
{
    ensure!(leaf_count <= MAX_LEAVES, "Invalid withdrawal queue depth");
    ensure!(
        nonce < leaf_count,
        "Withdrawal {nonce} has not been enqueued yet"
    );

    let index_bits = index_bits_le(nonce);
    let mut cache = BTreeMap::new();
    let mut siblings = [[0u8; 32]; TREE_DEPTH];

    for level in 0..TREE_DEPTH {
        let block_size = 1u64 << level;
        let block_index = nonce / block_size;
        let sibling_index = block_index ^ 1;
        let sibling_start = sibling_index.saturating_mul(block_size);

        siblings[level] = if sibling_start >= MAX_LEAVES {
            if level == 0 {
                ZERO_BYTES32
            } else {
                zero_hash(level - 1)
            }
        } else {
            compute_subtree_hash(level, sibling_start, leaf_count, &mut cache, fetch_leaf)?
        };
    }

    let root = compute_subtree_hash(TREE_DEPTH, 0, leaf_count, &mut cache, fetch_leaf)?;
    Ok(MerkleArtifacts {
        siblings,
        index_bits_le: index_bits,
        root,
    })
}

fn compute_subtree_hash<F>(
    level: usize,
    start_index: u64,
    leaf_count: u64,
    cache: &mut BTreeMap<(usize, u64), [u8; 32]>,
    fetch_leaf: &mut F,
) -> Result<[u8; 32]>
where
    F: FnMut(u64) -> Result<[u8; 32]>,
{
    if let Some(value) = cache.get(&(level, start_index)) {
        return Ok(*value);
    }

    let value = if start_index >= MAX_LEAVES {
        if level == 0 {
            ZERO_BYTES32
        } else {
            zero_hash(level - 1)
        }
    } else if level == 0 {
        if start_index < leaf_count {
            fetch_leaf(start_index)?
        } else {
            ZERO_BYTES32
        }
    } else {
        let half = 1u64 << (level - 1);
        let left = compute_subtree_hash(level - 1, start_index, leaf_count, cache, fetch_leaf)?;
        let right_start = start_index.checked_add(half).unwrap_or(MAX_LEAVES);
        let right = compute_subtree_hash(level - 1, right_start, leaf_count, cache, fetch_leaf)?;
        hash_merkle_node(&left, &right)
    };

    cache.insert((level, start_index), value);
    Ok(value)
}

/// Call messages accepted by [`MidnightWithdrawals`].
#[derive(Debug, PartialEq, Eq, Clone, JsonSchema, EnumDiscriminants, EnumIs, UniversalWallet)]
#[serialize(Borsh, Serde)]
#[schemars(rename = "MidnightWithdrawalsCallMessage")]
#[strum_discriminants(derive(VariantArray, EnumIs))]
#[serde(rename_all = "snake_case")]
pub enum CallMessage {
    /// Burn NIGHT on L2 and enqueue a message for L1 finalization.
    WithdrawNight {
        /// Midnight recipient identifier supplied by the user.
        midnight_address: MidnightAddress,
        /// Amount of NIGHT to withdraw.
        amount: Amount,
        /// Placeholder for future L1 relayer gas budgeting.
        gas_limit: Option<u64>,
    },
}

/// Events emitted by [`MidnightWithdrawals`].
#[derive(Debug, PartialEq, Eq, Clone, JsonSchema)]
#[serialize(Borsh, Serde)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// Emitted whenever a withdrawal request is queued.
    WithdrawalQueued {
        /// Sequential nonce assigned to the withdrawal.
        nonce: u64,
        /// Amount of NIGHT burned on L2.
        amount: Amount,
    },
}

impl<S: Spec> Module for MidnightWithdrawals<S> {
    type Spec = S;

    type Config = MidnightWithdrawalsConfig;

    type CallMessage = CallMessage;

    type Event = Event;

    fn genesis(
        &mut self,
        _genesis_rollup_header: &<<S as Spec>::Da as DaSpec>::BlockHeader,
        config: &Self::Config,
        state: &mut impl GenesisState<S>,
    ) -> anyhow::Result<()> {
        self.enabled.set(&config.enabled, state)?;
        self.message_count.set(&0, state)?;
        let initial_root = zero_hash(TREE_DEPTH - 1);
        self.withdraw_root.set(&initial_root, state)?;
        for level in 0..TREE_DEPTH {
            self.branch_state(level).set(&ZERO_BYTES32, state)?;
        }
        Ok(())
    }

    fn call(
        &mut self,
        msg: Self::CallMessage,
        context: &ModuleContext<Self::Spec>,
        state: &mut impl TxState<S>,
    ) -> anyhow::Result<()> {
        match msg {
            CallMessage::WithdrawNight {
                midnight_address,
                amount,
                gas_limit,
            } => self.withdraw_night(midnight_address, amount, gas_limit, context, state),
        }
    }
}

impl<S: Spec> MidnightWithdrawals<S> {
    fn withdraw_night(
        &mut self,
        midnight_address: MidnightAddress,
        amount: Amount,
        gas_limit: Option<u64>,
        context: &ModuleContext<S>,
        state: &mut impl TxState<S>,
    ) -> Result<()> {
        self.ensure_enabled(state)?;
        ensure!(amount > Amount::ZERO, "Withdrawal amount must be non-zero");

        let coins = Coins {
            amount,
            token_id: config_gas_token_id(),
        };

        self.bank
            .burn(coins.clone(), context.sender(), state)
            .context("Failed to burn NIGHT while initiating withdrawal")?;

        let nonce = self.current_nonce(state)?;
        let sender_bytes = Self::sender_bytes(context.sender())?;
        let recipient_bytes = Self::recipient_bytes(&midnight_address)?;
        let message = WithdrawMessage::new(sender_bytes, recipient_bytes, amount.0, nonce);
        let message_hash = hash_withdraw_message(&message);
        let leaf_hash = hash_withdraw_leaf(&message_hash);
        self.append_leaf(nonce, leaf_hash, state)?;

        let record = StoredWithdrawal {
            nonce,
            midnight_address,
            recipient_bytes,
            sender_bytes,
            amount,
            token_id: coins.token_id,
            l2_sender: context.sender().clone(),
            gas_limit,
            message_hash,
            leaf_hash,
        };
        self.withdrawals.set(&nonce, &record, state)?;

        self.emit_event(state, Event::WithdrawalQueued { nonce, amount });

        Ok(())
    }

    fn ensure_enabled(&self, state: &mut impl TxState<S>) -> Result<()> {
        let enabled = self.enabled.get(state)?.unwrap_or(false);
        ensure!(
            enabled,
            "Midnight withdrawals are disabled in genesis config"
        );
        Ok(())
    }

    fn current_nonce(&self, state: &mut impl TxState<S>) -> Result<u64> {
        Ok(self.message_count.get(state)?.unwrap_or(0))
    }

    fn append_leaf(
        &mut self,
        expected_index: u64,
        leaf_hash: [u8; 32],
        state: &mut impl TxState<S>,
    ) -> Result<()> {
        ensure!(
            expected_index < MAX_LEAVES,
            "Midnight withdrawal queue is full"
        );
        let index = self.message_count.get(state)?.unwrap_or(0);
        ensure!(
            index == expected_index,
            "Midnight withdrawal nonce mismatch: expected {}, found {}",
            expected_index,
            index
        );

        let mut current = leaf_hash;
        for level in 0..TREE_DEPTH {
            let bit_set = ((index >> level) & 1) == 1;
            let branch_state = self.branch_state(level);
            let branch_value = branch_state.get(state)?.unwrap_or(ZERO_BYTES32);
            if bit_set {
                current = hash_merkle_node(&branch_value, &current);
            } else {
                branch_state.set(&current, state)?;
                let zero = zero_sibling(level);
                current = hash_merkle_node(&current, &zero);
            }
        }

        self.withdraw_root.set(&current, state)?;
        let next = index
            .checked_add(1)
            .context("Midnight withdrawal nonce overflow")?;
        self.message_count.set(&next, state)?;
        Ok(())
    }

    #[allow(dead_code)]
    fn build_withdrawal_proof(
        &mut self,
        nonce: u64,
        state: &mut impl TxState<S>,
    ) -> Result<WithdrawalProofBundle<S>> {
        let total = self.message_count.get(state)?.unwrap_or(0);
        ensure!(nonce < total, "Withdrawal {nonce} is not available yet");
        let withdraw_root = self
            .withdraw_root
            .get(state)?
            .unwrap_or(zero_hash(TREE_DEPTH - 1));
        let record = self
            .withdrawals
            .get(&nonce, state)?
            .with_context(|| format!("Missing withdrawal record {nonce}"))?;

        let mut cached_leaves = BTreeMap::new();
        let mut fetch_leaf = |index: u64| -> Result<[u8; 32]> {
            if let Some(value) = cached_leaves.get(&index) {
                return Ok(*value);
            }
            if index >= total {
                return Ok(ZERO_BYTES32);
            }
            let leaf = self
                .withdrawals
                .get(&index, state)?
                .with_context(|| format!("Missing withdrawal record {index}"))?
                .leaf_hash;
            cached_leaves.insert(index, leaf);
            Ok(leaf)
        };

        let artifacts = compute_merkle_artifacts(nonce, total, &mut fetch_leaf)?;
        ensure!(
            artifacts.root == withdraw_root,
            "Reconstructed withdraw root mismatch while building proof"
        );

        Ok(WithdrawalProofBundle {
            record,
            siblings: artifacts.siblings,
            index_bits_le: artifacts.index_bits_le,
            leaf_count: total,
            withdraw_root,
        })
    }

    fn branch_state(&mut self, level: usize) -> &mut StateValue<[u8; 32]> {
        match level {
            0 => &mut self.branch_0,
            1 => &mut self.branch_1,
            2 => &mut self.branch_2,
            3 => &mut self.branch_3,
            4 => &mut self.branch_4,
            5 => &mut self.branch_5,
            6 => &mut self.branch_6,
            7 => &mut self.branch_7,
            8 => &mut self.branch_8,
            9 => &mut self.branch_9,
            10 => &mut self.branch_10,
            11 => &mut self.branch_11,
            12 => &mut self.branch_12,
            13 => &mut self.branch_13,
            14 => &mut self.branch_14,
            15 => &mut self.branch_15,
            _ => unreachable!("Invalid branch level {}", level),
        }
    }

    fn sender_bytes(address: &S::Address) -> Result<[u8; 32]> {
        let raw = address.as_ref();
        ensure!(
            raw.len() <= 32,
            "Sender address must be at most 32 bytes, got {}",
            raw.len()
        );
        let mut out = [0u8; 32];
        out[..raw.len()].copy_from_slice(raw);
        Ok(out)
    }

    fn recipient_bytes(address: &MidnightAddress) -> Result<[u8; 32]> {
        let raw = address.as_str();
        let trimmed = raw.strip_prefix("0x").unwrap_or(raw);
        let bytes = decode(trimmed).with_context(|| {
            format!(
                "Failed to decode Midnight recipient {}; expected hex-encoded bytes32",
                raw
            )
        })?;
        ensure!(
            bytes.len() == 32,
            "Midnight recipient must decode to exactly 32 bytes"
        );
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(out)
    }

    #[cfg(feature = "native")]
    fn record_to_response(record: StoredWithdrawal<S>) -> WithdrawalResponse {
        WithdrawalResponse {
            nonce: record.nonce,
            midnight_address: record.midnight_address,
            amount: record.amount,
            token_id: record.token_id,
            l2_sender_debug: format!("{:?}", record.l2_sender),
            gas_limit: record.gas_limit,
            sender_bytes_hex: encode(record.sender_bytes),
            recipient_bytes_hex: encode(record.recipient_bytes),
            message_hash_hex: encode(record.message_hash),
            leaf_hash_hex: encode(record.leaf_hash),
        }
    }
}

#[cfg(feature = "native")]
impl<S> MidnightWithdrawals<S>
where
    S: Spec,
    S::Address: Debug,
{
    async fn route_get_withdrawal(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
        Path(nonce): Path<u64>,
    ) -> ApiResult<WithdrawalResponse> {
        let Some(record) = state
            .withdrawals
            .get(&nonce, &mut accessor)
            .unwrap_infallible()
        else {
            return Err(errors::not_found_404("Midnight withdrawal", nonce));
        };
        Ok(Self::record_to_response(record).into())
    }

    async fn route_queue_status(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
    ) -> ApiResult<QueueStatusResponse> {
        let next_nonce = state
            .message_count
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(0);
        let withdraw_root = state
            .withdraw_root
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(zero_hash(TREE_DEPTH - 1));
        Ok(QueueStatusResponse {
            next_nonce,
            withdraw_root_hex: encode(withdraw_root),
        }
        .into())
    }

    async fn route_withdrawal_proof(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
        Path(nonce): Path<u64>,
        Query(params): Query<ProofQuery>,
    ) -> ApiResult<WithdrawalProofResponse> {
        let total = state
            .message_count
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(0);
        let Some(record) = state
            .withdrawals
            .get(&nonce, &mut accessor)
            .unwrap_infallible()
        else {
            return Err(errors::not_found_404("Midnight withdrawal", nonce));
        };
        if nonce >= total {
            return Err(errors::bad_request_400(
                "Withdrawal has not been enqueued yet",
                format!("nonce {nonce} >= message_count {total}"),
            ));
        }
        let withdraw_root = state
            .withdraw_root
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(zero_hash(TREE_DEPTH - 1));

        let mut cached_leaves = BTreeMap::new();
        let mut fetch_leaf = |index: u64| -> Result<[u8; 32]> {
            if let Some(value) = cached_leaves.get(&index) {
                return Ok(*value);
            }
            if index >= total {
                return Ok(ZERO_BYTES32);
            }
            let Some(withdrawal) = state
                .withdrawals
                .get(&index, &mut accessor)
                .unwrap_infallible()
            else {
                bail!("Missing withdrawal record {index}");
            };
            cached_leaves.insert(index, withdrawal.leaf_hash);
            Ok(withdrawal.leaf_hash)
        };

        let artifacts = compute_merkle_artifacts(nonce, total, &mut fetch_leaf)
            .map_err(|err| errors::internal_server_error_response_500(err))?;
        if artifacts.root != withdraw_root {
            return Err(errors::internal_server_error_response_500(
                "Merkle root mismatch while reconstructing proof",
            ));
        }

        let bundle = WithdrawalProofBundle {
            record,
            siblings: artifacts.siblings,
            index_bits_le: artifacts.index_bits_le,
            leaf_count: total,
            withdraw_root,
        };
        let response =
            WithdrawalProofResponse::from_bundle(bundle, params.batch_index.unwrap_or_default());
        Ok(response.into())
    }
}

#[cfg(feature = "native")]
impl<S> HasCustomRestApi for MidnightWithdrawals<S>
where
    S: Spec,
    S::Address: Debug,
{
    type Spec = S;

    fn custom_rest_api(&self, state: ApiState<S>) -> Router<()> {
        Router::new()
            .route("/withdrawals/:nonce", get(Self::route_get_withdrawal))
            .route(
                "/withdrawals/:nonce/proof",
                get(Self::route_withdrawal_proof),
            )
            .route("/withdrawals/queue", get(Self::route_queue_status))
            .with_state(state.with(self.clone()))
    }
}

#[cfg(feature = "native")]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
struct WithdrawalResponse {
    /// Sequential nonce assigned on L2.
    pub nonce: u64,
    /// Midnight recipient identifier supplied in the call.
    pub midnight_address: MidnightAddress,
    /// Amount of NIGHT the user burned.
    pub amount: Amount,
    /// Token identifier (currently always the canonical gas token).
    pub token_id: TokenId,
    /// Debug representation of the L2 sender, used as a lightweight proof.
    pub l2_sender_debug: String,
    /// Optional relayer gas limit hint attached by the user.
    pub gas_limit: Option<u64>,
    /// Hex encoding of the raw sender bytes used in hashing.
    pub sender_bytes_hex: String,
    /// Hex encoding of the raw Midnight recipient bytes.
    pub recipient_bytes_hex: String,
    /// Hex encoding of the withdrawal message hash.
    pub message_hash_hex: String,
    /// Hex encoding of the Merkle leaf hash.
    pub leaf_hash_hex: String,
}

#[cfg(feature = "native")]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
struct QueueStatusResponse {
    /// Next nonce that will be assigned to a withdrawal.
    pub next_nonce: u64,
    /// Hex encoding of the current withdraw root.
    pub withdraw_root_hex: String,
}

#[cfg(feature = "native")]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
struct ProofQuery {
    batch_index: Option<u64>,
}

#[cfg(feature = "native")]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
struct WithdrawalProofResponse {
    pub nonce: u64,
    pub midnight_address: MidnightAddress,
    pub amount: Amount,
    pub token_id: TokenId,
    pub l2_sender_debug: String,
    pub gas_limit: Option<u64>,
    pub sender_bytes_hex: String,
    pub recipient_bytes_hex: String,
    pub message_hash_hex: String,
    pub leaf_hash_hex: String,
    pub leaf_count: u64,
    pub withdraw_root_hex: String,
    pub l1_proof: L1WithdrawProof16Response,
}

#[cfg(feature = "native")]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
struct L1WithdrawProof16Response {
    pub batch_index: u64,
    pub nonce: u64,
    pub index_bits_le: [bool; TREE_DEPTH],
    pub sibling_hashes_hex: [String; TREE_DEPTH],
}

#[cfg(feature = "native")]
impl WithdrawalProofResponse {
    fn from_bundle<S>(bundle: WithdrawalProofBundle<S>, batch_index: u64) -> Self
    where
        S: Spec,
        S::Address: Debug,
    {
        let WithdrawalProofBundle {
            record,
            siblings,
            index_bits_le,
            leaf_count,
            withdraw_root,
        } = bundle;
        let l1_proof =
            L1WithdrawProof16Binary::new(batch_index, record.nonce, index_bits_le, siblings)
                .into_response();
        WithdrawalProofResponse {
            nonce: record.nonce,
            midnight_address: record.midnight_address,
            amount: record.amount,
            token_id: record.token_id,
            l2_sender_debug: format!("{:?}", record.l2_sender),
            gas_limit: record.gas_limit,
            sender_bytes_hex: encode(record.sender_bytes),
            recipient_bytes_hex: encode(record.recipient_bytes),
            message_hash_hex: encode(record.message_hash),
            leaf_hash_hex: encode(record.leaf_hash),
            leaf_count,
            withdraw_root_hex: encode(withdraw_root),
            l1_proof,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_leaf(index: u64) -> [u8; 32] {
        let sender = [index as u8; 32];
        let mut recipient = [0u8; 32];
        recipient.fill(index as u8 + 1);
        let msg = WithdrawMessage::new(sender, recipient, (index as u128) + 1, index);
        let message_hash = hash_withdraw_message(&msg);
        hash_withdraw_leaf(&message_hash)
    }

    fn compute_naive_root(leaves: &[[u8; 32]]) -> [u8; 32] {
        let mut level = vec![[0u8; 32]; MAX_LEAVES as usize];
        for (idx, leaf) in leaves.iter().enumerate() {
            level[idx] = *leaf;
        }
        let mut size = MAX_LEAVES as usize;
        while size > 1 {
            let mut next = 0;
            for i in 0..size / 2 {
                let left = level[2 * i];
                let right = level[2 * i + 1];
                level[next] = hash_merkle_node(&left, &right);
                next += 1;
            }
            size /= 2;
        }
        level[0]
    }

    #[test]
    fn merkle_artifacts_round_trip() {
        let total = 4u64;
        let leaves: Vec<[u8; 32]> = (0..total).map(sample_leaf).collect();
        let mut fetch = |idx: u64| -> Result<[u8; 32]> {
            Ok(*leaves.get(idx as usize).unwrap_or(&ZERO_BYTES32))
        };
        let artifacts = compute_merkle_artifacts(2, total, &mut fetch).unwrap();
        let naive_root = compute_naive_root(&leaves);
        assert_eq!(artifacts.root, naive_root);
        let mut acc = leaves[2];
        for level in 0..TREE_DEPTH {
            let sibling = artifacts.siblings[level];
            let (left, right) = if artifacts.index_bits_le[level] {
                (sibling, acc)
            } else {
                (acc, sibling)
            };
            acc = hash_merkle_node(&left, &right);
        }
        assert_eq!(acc, artifacts.root);
    }
}

//! Minimal Midnight L2 -> L1 withdrawal prototype.

use std::fmt::Debug;

use anyhow::{ensure, Context, Result};
use borsh::{BorshDeserialize, BorshSerialize};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sov_bank::{config_gas_token_id, Bank, Coins, TokenId};
use sov_modules_api::macros::{serialize, UniversalWallet};
use sov_modules_api::{
    Amount, Context as ModuleContext, DaSpec, EventEmitter, GenesisState, Module, ModuleId,
    ModuleInfo, ModuleRestApi, SafeString, Spec, StateMap, StateValue, TxState,
};
use strum::{EnumDiscriminants, EnumIs, VariantArray};

#[cfg(feature = "native")]
use sov_modules_api::prelude::axum::{self, routing::get, Router};
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
    /// Next nonce assigned to an outbound withdrawal message.
    #[state]
    pub next_nonce: StateValue<u64>,
    /// Feature flag controlled at genesis.
    #[state]
    pub enabled: StateValue<bool>,
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
    amount: Amount,
    token_id: TokenId,
    l2_sender: S::Address,
    gas_limit: Option<u64>,
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
        self.next_nonce.set(&0, state)?;
        self.enabled.set(&config.enabled, state)?;
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
        let next_nonce = nonce
            .checked_add(1)
            .context("Midnight withdrawal nonce overflow")?;
        self.next_nonce.set(&next_nonce, state)?;

        let record = StoredWithdrawal {
            nonce,
            midnight_address,
            amount,
            token_id: coins.token_id,
            l2_sender: context.sender().clone(),
            gas_limit,
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
        Ok(self.next_nonce.get(state)?.unwrap_or(0))
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

    async fn route_latest_nonce(
        state: ApiState<S, Self>,
        mut accessor: ApiStateAccessor<S>,
    ) -> ApiResult<LatestNonceResponse> {
        let next_nonce = state
            .next_nonce
            .get(&mut accessor)
            .unwrap_infallible()
            .unwrap_or(0);
        Ok(LatestNonceResponse { next_nonce }.into())
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
            .route(
                "/withdrawals/:nonce",
                get(Self::route_get_withdrawal),
            )
            .route(
                "/withdrawals/latest-nonce",
                get(Self::route_latest_nonce),
            )
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
}

#[cfg(feature = "native")]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
struct LatestNonceResponse {
    /// Next nonce that will be assigned to a withdrawal.
    pub next_nonce: u64,
}

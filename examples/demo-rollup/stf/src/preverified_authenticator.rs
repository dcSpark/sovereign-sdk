use std::marker::PhantomData;

use sov_address::{EthereumAddress, FromVmAddress};
use sov_evm::{EvmAuthenticator, EvmAuthenticatorInput};
use sov_modules_api::capabilities::{
    AuthenticationError, AuthenticationOutput, BatchFromUnregisteredSequencer, FatalError,
    TransactionAuthenticator, UnregisteredAuthenticationError,
};
use sov_modules_api::{
    DispatchCall, FullyBakedTx, GetGasPrice, ProvableStateReader, RawTx, Runtime, Spec,
};
use sov_rollup_interface::TxHash;
use sov_state::User;

/// Transaction authenticator wrapper that hydrates Midnight Privacy's pre-verified cache
/// from `worker_verified_transactions` (best-effort) using the tx hash.
///
/// This is critical for resync/replay when proof bytes are stripped from pre-authenticated txs:
/// the node must be able to skip Ligero verification and use persisted proof outputs instead.
pub struct PreverifiedEvmAuthenticator<S, Rt>(PhantomData<(S, Rt)>);

impl<S, Rt> TransactionAuthenticator<S> for PreverifiedEvmAuthenticator<S, Rt>
where
    S: Spec,
    S::Address: FromVmAddress<EthereumAddress>,
    Rt: Runtime<S> + DispatchCall<Spec = S>,
{
    type Decodable = EvmAuthenticatorInput<sov_evm::CallMessage, <Rt as DispatchCall>::Decodable>;
    type Input = EvmAuthenticatorInput;

    fn authenticate<Accessor: ProvableStateReader<User, Spec = S> + GetGasPrice<Spec = S>>(
        tx: &FullyBakedTx,
        state: &mut Accessor,
    ) -> Result<AuthenticationOutput<S, Self::Decodable>, AuthenticationError> {
        let (tx_and_raw_hash, auth_data, runtime_call) =
            EvmAuthenticator::<S, Rt>::authenticate(tx, state)?;

        #[cfg(feature = "native")]
        {
            midnight_privacy::prime_pre_verified_spend(&tx_and_raw_hash.raw_tx_hash);
        }

        Ok((tx_and_raw_hash, auth_data, runtime_call))
    }

    #[cfg(feature = "native")]
    fn compute_tx_hash(tx: &FullyBakedTx) -> anyhow::Result<TxHash> {
        EvmAuthenticator::<S, Rt>::compute_tx_hash(tx)
    }

    #[cfg(feature = "native")]
    fn decode_serialized_tx(tx: &FullyBakedTx) -> Result<Self::Decodable, FatalError> {
        EvmAuthenticator::<S, Rt>::decode_serialized_tx(tx)
    }

    fn authenticate_unregistered<Accessor: ProvableStateReader<User, Spec = S>>(
        batch: &BatchFromUnregisteredSequencer,
        state: &mut Accessor,
    ) -> Result<AuthenticationOutput<S, Self::Decodable>, UnregisteredAuthenticationError> {
        EvmAuthenticator::<S, Rt>::authenticate_unregistered(batch, state)
    }

    fn add_standard_auth(tx: RawTx) -> Self::Input {
        EvmAuthenticator::<S, Rt>::add_standard_auth(tx)
    }

    fn encode_with_pre_authenticated(tx: RawTx, original_hash: TxHash) -> FullyBakedTx {
        <EvmAuthenticator<S, Rt> as TransactionAuthenticator<S>>::encode_with_pre_authenticated(
            tx,
            original_hash,
        )
    }
}

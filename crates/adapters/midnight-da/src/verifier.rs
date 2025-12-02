use borsh::{BorshDeserialize, BorshSerialize};
use sov_rollup_interface::da::{
    BlobReaderTrait, DaSpec, DaVerifier, RelevantBlobs, RelevantProofs,
};

use crate::{MidnightAddress, MidnightBlob, MidnightBlockHeader, MidnightDaVerifier, MidnightHash};

impl BlobReaderTrait for MidnightBlob {
    type Address = MidnightAddress;
    type BlobHash = MidnightHash;

    fn sender(&self) -> Self::Address {
        self.address
    }

    fn hash(&self) -> Self::BlobHash {
        self.hash
    }

    fn verified_data(&self) -> &[u8] {
        self.blob.accumulator()
    }

    fn total_len(&self) -> usize {
        self.blob.total_len()
    }

    fn advance(&mut self, num_bytes: usize) -> &[u8] {
        self.blob.advance(num_bytes);
        self.verified_data()
    }
}

/// A [`sov_rollup_interface::da::DaSpec`] suitable for testing.
#[derive(
    Default,
    serde::Serialize,
    serde::Deserialize,
    BorshSerialize,
    BorshDeserialize,
    Debug,
    PartialEq,
    Eq,
    Clone,
    schemars::JsonSchema,
)]
pub struct MidnightDaSpec;

impl DaSpec for MidnightDaSpec {
    type SlotHash = MidnightHash;
    type BlockHeader = MidnightBlockHeader;
    type BlobTransaction = MidnightBlob;
    type TransactionId = MidnightHash;
    type Address = MidnightAddress;

    type InclusionMultiProof = [u8; 32];
    type CompletenessProof = ();
    type ChainParams = ();
}

impl DaVerifier for MidnightDaVerifier {
    type Spec = MidnightDaSpec;

    type Error = anyhow::Error;

    fn new(_params: <Self::Spec as DaSpec>::ChainParams) -> Self {
        Self {}
    }

    fn verify_relevant_tx_list(
        &self,
        _block_header: &<Self::Spec as DaSpec>::BlockHeader,
        _relevant_blobs: &RelevantBlobs<<Self::Spec as DaSpec>::BlobTransaction>,
        _relevant_proofs: RelevantProofs<
            <Self::Spec as DaSpec>::InclusionMultiProof,
            <Self::Spec as DaSpec>::CompletenessProof,
        >,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

//! Utilities for handling TEE attestations.
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use tee::common::BatchPublicDataV1;

/// 32-byte domain separator for oracle-signed TEE attestation statements (v1).
///
/// This value is included in the signed message to prevent cross-protocol signature reuse.
pub const TEE_ORACLE_STATEMENT_DOMAIN_V1: [u8; 32] =
    *b"SOV_TEE_ORACLE_STATEMENT_V1\0\0\0\0\0";

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, Default, PartialEq, Eq)]
/// Represents a TEE attestation along with its batch data.
pub struct TEEAttestation {
    /// Serialized attestation.
    pub attestation: Vec<u8>,
    /// Raw aggregated proof bytes.
    pub raw_aggregated_proof: Vec<u8>,
    /// Batch data
    pub batch_data: BatchPublicDataV1,
    /// Type of the attestation.
    pub attestation_type: TEEAttestationType,
}

#[derive(
    Default, Debug, Eq, PartialEq, BorshDeserialize, BorshSerialize, Serialize, Deserialize, Clone,
)]
/// Shows what type of TEE attestation it is.
pub enum TEEAttestationType {
    #[default]
    /// Microsoft Azure Attestation
    MAA,
    /// Raw AMD SEV-SNP Attestation
    RawSevSnp,
}

/// A deterministic, oracle-signed statement binding a TEE attestation to the batch public data (v1).
///
/// The rollup verifies this statement purely from DA-provided bytes plus on-chain configured
/// oracle public keys (no HTTP calls, no external binaries).
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Eq)]
pub struct TeeOracleStatementV1 {
    /// Domain separator; must equal [`TEE_ORACLE_STATEMENT_DOMAIN_V1`].
    pub domain: [u8; 32],
    /// Type of the TEE attestation that was verified by the oracle.
    pub attestation_type: TEEAttestationType,
    /// The batch public data this attestation commits to.
    pub batch_data: BatchPublicDataV1,
    /// SHA-256 hash of `TEEAttestation.raw_aggregated_proof`.
    pub raw_aggregated_proof_sha256: [u8; 32],
    /// SHA-256 hash of the attestation JWT bytes (UTF-8 string bytes).
    pub attestation_jwt_sha256: [u8; 32],
}

/// Oracle request for signing a TEE attestation statement (v1).
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Eq)]
pub struct OracleAttestRequestV1 {
    /// The attestation JWT (typically MAA).
    pub attestation_jwt: String,
    /// The statement to sign.
    pub statement: TeeOracleStatementV1,
}

/// Oracle response containing a signature over the statement (v1).
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Eq)]
pub struct OracleAttestResponseV1 {
    /// The oracle Ed25519 verifying key (32 bytes).
    pub oracle_pubkey: [u8; 32],
    /// Ed25519 signature over `borsh(statement)` (64 bytes).
    pub oracle_signature: [u8; 64],
}

/// Oracle-signed attestation payload stored inside `TEEAttestation.attestation` (v1).
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Eq)]
pub struct TeeOracleSignedMAAAttestationV1 {
    /// The attestation JWT (MAA).
    pub attestation_jwt: String,
    /// The signed statement (must match outer fields).
    pub statement: TeeOracleStatementV1,
    /// The oracle Ed25519 verifying key (32 bytes).
    pub oracle_pubkey: [u8; 32],
    /// Ed25519 signature over `borsh(statement)` (64 bytes).
    pub oracle_signature: [u8; 64],
}

/// Represents a serialized TEE attestation.
#[derive(Debug, Eq, PartialEq, BorshDeserialize, BorshSerialize, Serialize, Deserialize, Clone)]
pub struct SerializedTEEAttestation {
    /// Serialized TEE attestation.
    pub tee_raw_attestation: Vec<u8>,
}

impl SerializedTEEAttestation {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_attestation_serialization() {
        let maa_attestation = SerializedTEEAttestation {
            tee_raw_attestation: borsh::to_vec("eyJhbGciOiJSUzI1NiIsImprdSI6Imh0dHBzOi8vc2hhcmVkZXVzMi5ldXMyLmF0dGVzdC5henVyZS5uZXQvY2VydHMiLCJraWQiOiJKMHBBUGRmWFhIcVdXaW1nckg4NTN3TUlkaDUvZkxlMXo2dVNYWVBYQ2EwPSIsInR5cCI6IkpXVCJ9.eyJleHAiOjE3Njc0ODgxODIsImlhdCI6MTc2NzQ1OTM4MiwiaXNzIjoiaHR0cHM6Ly9zaGFyZWRldXMyLmV1czIuYXR0ZXN0LmF6dXJlLm5ldCIsImp0aSI6IjY2NjliZDE2MTJmOTJlYTI2NzUyNmEyOTA1NzlkMWQzMWUwNDg1MGJkZDNjMDI5OTcyNjg0MWIyM2FhYWY3Y2YiLCJuYmYiOjE3Njc0NTkzODIsInNlY3VyZWJvb3QiOnRydWUsIngtbXMtYXR0ZXN0YXRpb24tdHlwZSI6ImF6dXJldm0iLCJ4LW1zLWF6dXJldm0tYXR0ZXN0YXRpb24tcHJvdG9jb2wtdmVyIjoiMy4wIiwieC1tcy1henVyZXZtLWF0dGVzdGVkLXBjcnMiOlswLDEsMiwzLDQsNSw2LDddLCJ4LW1zLWF6dXJldm0tYm9vdGRlYnVnLWVuYWJsZWQiOmZhbHNlLCJ4LW1zLWF6dXJldm0tZGJ2YWxpZGF0ZWQiOnRydWUsIngtbXMtYXp1cmV2bS1kYnh2YWxpZGF0ZWQiOnRydWUsIngtbXMtYXp1cmV2bS1kZWJ1Z2dlcnNkaXNhYmxlZCI6dHJ1ZSwieC1tcy1henVyZXZtLWRlZmF1bHQtc2VjdXJlYm9vdGtleXN2YWxpZGF0ZWQiOnRydWUsIngtbXMtYXp1cmV2bS1lbGFtLWVuYWJsZWQiOmZhbHNlLCJ4LW1zLWF6dXJldm0tZmxpZ2h0c2lnbmluZy1lbmFibGVkIjpmYWxzZSwieC1tcy1henVyZXZtLWh2Y2ktcG9saWN5IjowLCJ4LW1zLWF6dXJldm0taHlwZXJ2aXNvcmRlYnVnLWVuYWJsZWQiOmZhbHNlLCJ4LW1zLWF6dXJldm0taXMtd2luZG93cyI6ZmFsc2UsIngtbXMtYXp1cmV2bS1rZXJuZWxkZWJ1Zy1lbmFibGVkIjpmYWxzZSwieC1tcy1henVyZXZtLW9zYnVpbGQiOiJOb3RBcHBsaWNhdGlvbiIsIngtbXMtYXp1cmV2bS1vc2Rpc3RybyI6IlVidW50dSIsIngtbXMtYXp1cmV2bS1vc3R5cGUiOiJMaW51eCIsIngtbXMtYXp1cmV2bS1vc3ZlcnNpb24tbWFqb3IiOjIyLCJ4LW1zLWF6dXJldm0tb3N2ZXJzaW9uLW1pbm9yIjo0LCJ4LW1zLWF6dXJldm0tc2lnbmluZ2Rpc2FibGVkIjp0cnVlLCJ4LW1zLWF6dXJldm0tdGVzdHNpZ25pbmctZW5hYmxlZCI6ZmFsc2UsIngtbXMtYXp1cmV2bS12bWlkIjoiNUVDQjUyQUItRTdBNS00ODlELUIwNzUtQjJEMDQ3MUI2MEJEIiwieC1tcy1pc29sYXRpb24tdGVlIjp7IngtbXMtYXR0ZXN0YXRpb24tdHlwZSI6InNldnNucHZtIiwieC1tcy1jb21wbGlhbmNlLXN0YXR1cyI6ImF6dXJlLWNvbXBsaWFudC1jdm0iLCJ4LW1zLXJ1bnRpbWUiOnsia2V5cyI6W3siZSI6IkFRQUIiLCJrZXlfb3BzIjpbInNpZ24iXSwia2lkIjoiSENMQWtQdWIiLCJrdHkiOiJSU0EiLCJuIjoidy1WaFhnQUExcmlVWG53a2J6anV1ZW9rLXpaaURuZUZaZ0RzUF9FQWt1Vm5BS2dDSV93SlZMM3UzY3pzOElhUzZkUWN3V014aDdJdzlBdWZNNG94WlBvbjdXVi1wVTV6Mng4YjRMQnctdzVBTTliYWRhTW4xeFpoa1VSTC1jMzNSQjNYVGxXbWZoMHNzWmdzempyZkE2STV6LUJ3MjR3UE5EZi1KRVZfNDZ5YlpuVGZjNWI1SFVXdG9mT2hscTRmSXM1ZlFhVnQxLTgtTklRLXEwd1VrRjc0b0Z3Z0Z6MzBFR1hkdDZXcTQxd2pSNFV3UjZwN3B1VFFjNjBoWXhCeHJWVkhJZG1ydHNfOFBGaEdOUG45UkNFS1hrUFVfYWZaRm1td3M0RlBwYXRYVjBIQ0s5WGlQUE5Gd1FlQUE0a0FGR25ENXZOeV9SUDZUWjN6Q3hueVZ3In0seyJlIjoiQVFBQiIsImtleV9vcHMiOlsiZW5jcnlwdCJdLCJraWQiOiJIQ0xFa1B1YiIsImt0eSI6IlJTQSIsIm4iOiJyR215aUFBQV8yUXYwWWJ4SkQ1QUlWcHIwNlg1Vms0cWpIVXV1TGE5WmcxVEFERTV3OTFSVnZ0NWpZZjhGZHMyZkZtd01Fa2s0cUJlYXJibGF3dFdlTnlwdndHZzhuMVNPdU5MRzFLOC16NVVVSy1IUktYTnVLLUljYWRPOVpqcUFOaUx2UEMwLUh6anRwTW42S2RiSWdkQUFROThfRE42UGc5OHlxRE9PX0VOSDF6QTBESmJRRFJyV1RQR0NUQnAzYUR6dUFBZGFRUF8ybEM0bGQ0WUt4TldOeGw3bGhMOVBaQllxaFZiQmdQV19uMnlSMGpWR3JTNlEzN2hqZEZJeXEtLXVRbkdlOVpWQ21pSFZ2R3dzX0Vvd1BoYzVmajRaY0pKY0lfblViU2loVEo0aXBUc3YxeHp2WlB0czZ6X2tCR3huVjhRNVdDRlVGZFA1UTd3YlEifV0sInVzZXItZGF0YSI6IjAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwIiwidm0tY29uZmlndXJhdGlvbiI6eyJjb25zb2xlLWVuYWJsZWQiOnRydWUsInNlY3VyZS1ib290Ijp0cnVlLCJ0cG0tZW5hYmxlZCI6dHJ1ZSwidm1VbmlxdWVJZCI6IjVFQ0I1MkFCLUU3QTUtNDg5RC1CMDc1LUIyRDA0NzFCNjBCRCJ9fSwieC1tcy1zZXZzbnB2bS1hdXRob3JrZXlkaWdlc3QiOiIwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAiLCJ4LW1zLXNldnNucHZtLWJvb3Rsb2FkZXItc3ZuIjo0LCJ4LW1zLXNldnNucHZtLWNpcGhlcnRleHQtaGlkaW5nLWRyYW0tZW5hYmxlZCI6ZmFsc2UsIngtbXMtc2V2c25wdm0tY3hsLWFsbG93ZWQiOmZhbHNlLCJ4LW1zLXNldnNucHZtLWZhbWlseUlkIjoiMDIyMTIwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAiLCJ4LW1zLXNldnNucHZtLWd1ZXN0c3ZuIjoxMCwieC1tcy1zZXZzbnB2bS1ob3N0ZGF0YSI6IjAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAiLCJ4LW1zLXNldnNucHZtLWlka2V5ZGlnZXN0IjoiOTQyZmQ5M2ViZGU2ZWE3YTk2ZWZhZGVhZmM2MGYxYzZiM2QxMGU3MDNiMWRhZmQ3NTU1YjkyZjdmM2QzMmQwZTAwNjc2NzY0OGNiYTViMTAyYWYzZDY1NzU2YWY0MTc3IiwieC1tcy1zZXZzbnB2bS1pbWFnZUlkIjoiMDIwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAiLCJ4LW1zLXNldnNucHZtLWlzLWRlYnVnZ2FibGUiOmZhbHNlLCJ4LW1zLXNldnNucHZtLWxhdW5jaG1lYXN1cmVtZW50IjoiNmEwNjNiZTlkZDc5ZjYzNzFjODQyZTQ4MGY4ZGMzYjVjNzI1OTYxMzQ0ZTU3MTMwZTg4YzVhZGY0OWU4ZjdmNmM3OWI3NWE1ZWI3N2ZjNzY5OTU5ZjRhZWIyZjk0MDFlIiwieC1tcy1zZXZzbnB2bS1tZW0tYWVzMjU2LXh0cy1yZXF1aXJlZCI6ZmFsc2UsIngtbXMtc2V2c25wdm0tbWljcm9jb2RlLXN2biI6MjE5LCJ4LW1zLXNldnNucHZtLW1pZ3JhdGlvbi1hbGxvd2VkIjpmYWxzZSwieC1tcy1zZXZzbnB2bS1wYWdlLXN3YXAtZGlzYWJsZWQiOmZhbHNlLCJ4LW1zLXNldnNucHZtLXJhcGwtZGlzYWJsZWQiOmZhbHNlLCJ4LW1zLXNldnNucHZtLXJlcG9ydGRhdGEiOiJmYWIzZjdjMGFkYjJjMGM1Zjg4Y2MzZTI5YTg4NGZkMDM1MzliN2YyZmFjMDQ2OTcxZThiZjMyZjI5NjhkOTY4MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMCIsIngtbXMtc2V2c25wdm0tcmVwb3J0aWQiOiJhMjFjNzA2MTU1MWY3MGRjMWIwY2ZiNTEyYTM3ZTBjY2Q3OTliZWVhYTIwMWFjMDY2ODk3MDExNjIzZTFmMzRiIiwieC1tcy1zZXZzbnB2bS1zaW5nbGVzb2NrZXQiOmZhbHNlLCJ4LW1zLXNldnNucHZtLXNtdC1hbGxvd2VkIjp0cnVlLCJ4LW1zLXNldnNucHZtLXNucGZ3LXN2biI6MjQsIngtbXMtc2V2c25wdm0tdGVlLXN2biI6MCwieC1tcy1zZXZzbnB2bS12bXBsIjowfSwieC1tcy1wb2xpY3ktaGFzaCI6IlFzTmk5VUUzNjVTZ2Nza3VWX2E0cC11RlF5R2xOY0lsUWtyc2kwVUl5N28iLCJ4LW1zLXJ1bnRpbWUiOnsiY2xpZW50LXBheWxvYWQiOnsibWlkbmlnaHRfcGF5bG9hZCI6IlFWRkJRVUZCUlVGQlFVRkJRVUZCUVVGUlFVRkJRVUZCUVVGQ2EwRkJRVUZCUVVGQlFVMW5RVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJjMDFuUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVGQlFVRkJRVUZCUVVOQlowbERRV2RKUTBGblNVTkJaMGxEUVdkSlEwRm5TVU5CWjBsRFFXZEpRMEZuU1VOQlowbERRV2M5Iiwibm9uY2UiOiJiV2xrYm1sbmFIUXRiREk9In0sImtleXMiOlt7ImUiOiJBUUFCIiwia2V5X29wcyI6WyJlbmNyeXB0Il0sImtpZCI6IlRwbUVwaGVtZXJhbEVuY3J5cHRpb25LZXkiLCJrdHkiOiJSU0EiLCJuIjoidXdabjRnQUE0MDdOOU16ZzRzSUx0RzlBdmFSX042bW8tNWdRMVJoTHZPVWc5U0g2eEVyYVVCN0ktZ1M3eWZfVmd0cENqeUV3X1lsRDA4eERPNHlFNFBsWXQxQVlocTFMSElTRnluSkUycHotTlZLNDNIX2RyRmZVenVEWF9ZT3Y0Z2dJNUIxUnJ1b1Q5WGk2QnpaLU5lUnpkd1J6OWxXRjFOVEZMdEJSWTE4SnpMSlU0R0JrMURWZ3kyd3N3T0lBMExubFZwLWQ5eEFEaXVuZzQ5NDV5NGlQa1l3Qk9zdldoeldzd1FVdjFRaGRmOE5TRVdjOWNjU3loM1VjVVNIUE4wdDlQOTVMclR3TTRVa0JvUW9XSThRRlJNYVMwRlFEeE0tQ0Zwdi1oWTlNT0NXNDJ5cks2V2UzR2NhSFBoaXVDNzJaRlN5b05EZTVFblhpdE5kU29RIn1dfSwieC1tcy12ZXIiOiIxLjAifQ.Xr9JewFIVZ8wrlPwxd30ex8bxfC-mpx6dlEJkTm3LQ-x9ie7UnybJ5GypyJgiksRrGVhhq4YtSQX9z2YjUViU8Ky3TutQCQUBSwlUCP83LEMA929p-5rejOC6rNstJrMQaRvyQMwSB60dmyerYfxG2GQkukCB7zHOGlw41PyKQUhbVmV0VFYA7uWJs7dY68vO_oO1Bx6UgCrqm-oa4OddCNn5mbOxiDsB1qrZRplGvBliT4k5fz_NZdX5tGkfTDAIfjvA0Tp4jgeM56rytAclA36nRQglRKnkto9e7RasJTrIlb-xo4rFxQrSzA-aRk403OgoawUd7JEZOE-d4PhNw").unwrap(),
        };
    }
}

extern crate base64 as b64;
use b64::prelude::*;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{DecodingKey, TokenData};
use openssl::bn::BigNum;
use ring::aead::{Aad, Nonce, Tag, UnboundKey, AES_256_GCM};
use serde_json::Value;
use tracing::info;
use webpki::types::{CertificateDer, TrustAnchor};
mod base64;
mod base64_str;
mod base64url;
mod json_base64;
mod json_base64url;

mod maa_jwt;
use std::path::Path;
use std::process::Command;

use crate::maa::attestation::maa_jwt::MaaClaims;
use ::base64::engine::general_purpose::URL_SAFE_NO_PAD;
use anyhow::{bail, Context as _};
use binrw::binrw;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Root {
    #[serde(with = "json_base64url")]
    pub attestation_info: AttestationInfo,
}
#[binrw]
#[brw(big)]
struct Tpm2bAttest {
    #[bw(try_calc(u16::try_from(attestation_data.len())))]
    size: u16,
    #[br(count = size)]
    attestation_data: Vec<u8>,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Proof {
    #[serde(with = "base64url")]
    pub snp_report: Vec<u8>,
    #[serde(with = "base64_str")]
    pub vcek_cert_chain: String,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AttestationInfo {
    pub attestation_protocol_version: String,
    pub client_payload: ClientPayload,
    pub isolation_info: IsolationInfo,
    #[serde(rename = "OSBuild")]
    #[serde(with = "base64_str")]
    pub os_build: String,
    #[serde(rename = "OSDistro")]
    #[serde(with = "base64_str")]
    pub os_distro: String,
    #[serde(rename = "OSType")]
    #[serde(with = "base64_str")]
    pub os_type: String,
    #[serde(rename = "OSVersionMajor")]
    pub os_version_major: i64,
    #[serde(rename = "OSVersionMinor")]
    pub os_version_minor: i64,
    #[serde(with = "base64")]
    pub tcg_logs: Vec<u8>,
    pub tpm_info: TpmInfo,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientPayload {
    pub nonce: String,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct IsolationInfo {
    pub evidence: Evidence,
    #[serde(rename = "Type")]
    pub type_field: String,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Evidence {
    #[serde(with = "json_base64")]
    pub proof: Proof,
    #[serde(with = "base64")]
    pub run_time_data: Vec<u8>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct TpmInfo {
    #[serde(with = "base64")]
    pub aik_cert: Vec<u8>,
    #[serde(with = "base64")]
    pub aik_pub: Vec<u8>,
    #[serde(with = "base64")]
    pub enc_key_certify_info: Vec<u8>,
    #[serde(with = "base64")]
    pub enc_key_certify_info_signature: Vec<u8>,
    #[serde(with = "base64")]
    pub enc_key_pub: Vec<u8>,
    #[serde(rename = "PCRs")]
    pub pcrs: Vec<Pcr>,
    #[serde(with = "base64")]
    pub pcr_quote: Vec<u8>,
    pub pcr_set: Vec<u8>,
    #[serde(with = "base64")]
    pub pcr_signature: Vec<u8>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Pcr {
    #[serde(with = "base64")]
    pub digest: Vec<u8>,
    pub index: u8,
}

#[derive(Serialize, Deserialize)]
pub struct MaaResponse {
    #[serde(with = "json_base64url")]
    token: EncryptedJwt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct EncryptedJwt {
    #[serde(with = "base64")]
    pub jwt: Vec<u8>,
    #[serde(with = "base64")]
    pub encrypted_inner_key: Vec<u8>,
    pub encryption_params: EncryptionParams,
    #[serde(with = "base64")]
    pub authentication_data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BlockMode {
    #[serde(rename = "ChainingModeGCM")]
    ChainingModeGcm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BlockCipherPadding {
    PKCS7,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CipherAlgorithm {
    AES,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct EncryptionParams {
    pub block_mode: BlockMode,
    pub block_padding: BlockCipherPadding,
    pub cipher: CipherAlgorithm,
    pub key_size_in_bits: u64,
    #[serde(with = "base64")]
    pub iv: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
pub struct ResponseStruct {
    maa_resp: MaaResponse,
    key: Vec<u8>,
    msg: String,
    sig: String,
    pcr: String,
}

use openssl::rsa;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use tempfile::NamedTempFile;


const MAA_ATTESTATION_URL: &str = "https://midnightl2.eus.attest.azure.net"
const APPLICATION_PCR_INDEX: u32 = 15;
const ATTESTATION_DOCUMENT_URL: &str =
    "https://attestation-endpoint.api.mydapp.midnight.io/attestation.json";

#[derive(Deserialize, Debug)]
struct StaticAttestationDocument {
    webserver_rootca: String,
    application_disk_roothash: String,
}

fn decode_sha256_hex(hex: &str) -> anyhow::Result<[u8; 32]> {
    let hex = hex.trim().trim_start_matches("0x");
    if hex.len() != 64 {
        bail!(
            "expected a 32-byte SHA-256 hex digest, got {} hex chars",
            hex.len()
        );
    }

    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let start = i * 2;
        *byte = u8::from_str_radix(&hex[start..start + 2], 16)
            .with_context(|| format!("invalid hex byte at offset {}", start))?;
    }
    Ok(out)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn tpm_extend(current_pcr: [u8; 32], digest: [u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(current_pcr);
    hasher.update(digest);
    hasher.finalize().into()
}

fn calculate_expected_pcr15(
    application_root_hash_hex: &str,
    webserver_rootca: &str,
) -> anyhow::Result<String> {
    let application_root_hash = decode_sha256_hex(application_root_hash_hex)?;
    let zero_digest = [0u8; 32];
    let caddy_rootca_hash: [u8; 32] = Sha256::digest(webserver_rootca.as_bytes()).into();

    let mut pcr = [0u8; 32];
    pcr = tpm_extend(pcr, application_root_hash);
    pcr = tpm_extend(pcr, zero_digest);
    pcr = tpm_extend(pcr, caddy_rootca_hash);

    Ok(encode_hex(&pcr))
}

fn attestation_document_url() -> anyhow::Result<reqwest::Url> {
    reqwest::Url::parse(ATTESTATION_DOCUMENT_URL).with_context(|| {
        format!(
            "invalid attestation document URL: {}",
            ATTESTATION_DOCUMENT_URL
        )
    })
}

fn attestation_document_resolve_addr(
    document_url: &reqwest::Url,
    dapp_attestation_url: &str,
) -> anyhow::Result<SocketAddr> {
    let dapp_url = reqwest::Url::parse(dapp_attestation_url)
        .with_context(|| format!("invalid DApp attestation URL: {}", dapp_attestation_url))?;
    let dapp_host = dapp_url
        .host_str()
        .with_context(|| format!("DApp attestation URL has no host: {}", dapp_attestation_url))?;
    let ip: IpAddr = dapp_host.parse().with_context(|| {
        format!(
            "DApp attestation URL host must be an IP address for attestation document resolution: {}",
            dapp_host
        )
    })?;
    let port = document_url.port_or_known_default().with_context(|| {
        format!(
            "attestation document URL has no explicit or known-default port: {}",
            document_url
        )
    })?;
    Ok(SocketAddr::new(ip, port))
}

async fn fetch_static_attestation_document(
    dapp_attestation_url: &str,
) -> anyhow::Result<StaticAttestationDocument> {
    let url = attestation_document_url()?;
    let host = url
        .host_str()
        .with_context(|| format!("attestation document URL has no host: {}", url))?;
    let resolve_addr = attestation_document_resolve_addr(&url, dapp_attestation_url)?;

    let response: StaticAttestationDocument = Client::builder()
        .danger_accept_invalid_certs(true)
        .resolve(host, resolve_addr)
        .build()
        .context("failed to build attestation document HTTP client")?
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("failed to fetch attestation document from {}", url))?
        .error_for_status()
        .with_context(|| format!("attestation document endpoint returned an error: {}", url))?
        .json()
        .await
        .with_context(|| format!("failed to parse attestation document from {}", url))?;

    if response.webserver_rootca.is_empty() {
        bail!("attestation document has an empty webserver_rootca");
    }
    decode_sha256_hex(&response.application_disk_roothash)
        .context("attestation document has an invalid application_disk_roothash")?;
    Ok(response)
}

fn verify_application_pcr15(
    pcr_map: &std::collections::HashMap<u32, String>,
    policy: &Value,
    attestation_doc: &StaticAttestationDocument,
) -> anyhow::Result<()> {
    let application_root_hash = policy
        .get("application_root_hash")
        .and_then(Value::as_str)
        .context("policy missing string key \"application_root_hash\"")?;

    let expected_application_root_hash = decode_sha256_hex(application_root_hash)
        .context("policy has an invalid application_root_hash")?;
    let attested_application_root_hash =
        decode_sha256_hex(&attestation_doc.application_disk_roothash)
            .context("attestation document has an invalid application_disk_roothash")?;

    if attested_application_root_hash != expected_application_root_hash {
        bail!(
            "application root hash mismatch\n  expected: {}\n  got     : {}",
            application_root_hash,
            attestation_doc.application_disk_roothash
        );
    }

    let expected_pcr15 =
        calculate_expected_pcr15(application_root_hash, &attestation_doc.webserver_rootca)
            .context("failed to calculate expected PCR15")?;

    info!(
        "Expected PCR {} value: {}",
        APPLICATION_PCR_INDEX, expected_pcr15
    );
    info!("{:?}", attestation_doc);
    let actual_pcr15 = pcr_map
        .get(&APPLICATION_PCR_INDEX)
        .with_context(|| format!("TPM quote is missing PCR {}", APPLICATION_PCR_INDEX))?
        .trim_start_matches("0x")
        .to_lowercase();

    if actual_pcr15 != expected_pcr15 {
        bail!(
            "PCR {} digest mismatch\n  expected: {}\n  got     : {}",
            APPLICATION_PCR_INDEX,
            expected_pcr15,
            actual_pcr15
        );
    }

    tracing::info!(
        "PCR {} verification passed: {}",
        APPLICATION_PCR_INDEX,
        actual_pcr15
    );
    Ok(())
}

/// Compare PCR against the expected values declared in the policy's measurements object.
fn verify_pcr_values(
    pcr_map: &std::collections::HashMap<u32, String>,
    expected_measurements: &Value,
) -> anyhow::Result<()> {
    let expected_map = expected_measurements
        .as_object()
        .context("policy measurements must be a JSON object")?;

    for (key, expected_val) in expected_map {
        let idx: u32 = key
            .parse()
            .with_context(|| format!("invalid PCR index key in policy measurements: {}", key))?;

        let actual_hex = pcr_map.get(&idx).with_context(|| {
            format!(
                "policy requires PCR {} but it was not in the tpm2_checkquote output",
                idx
            )
        })?;

        let expected_hex = expected_val
            .as_str()
            .with_context(|| format!("policy measurements[\"{}\"] must be a string", key))?;

        let expected_hex = expected_hex.trim_start_matches("0x").to_lowercase();
        let actual_hex = actual_hex.trim_start_matches("0x").to_lowercase();

        if actual_hex != expected_hex {
            bail!(
                "PCR {} digest mismatch\n  expected: {}\n  got     : {}",
                idx,
                expected_hex,
                actual_hex
            );
        }
        tracing::info!("PCR {} verification passed: {}", idx, actual_hex);
    }

    tracing::info!("All declared PCR measurements verified successfully");
    Ok(())
}

/// Run tpm2_checkquote, verify the TPM quote, and return the PCR map parsed from its output
fn verify_measurements(
    msg_file: &Path,
    sig_file: &Path,
    pcr_file: &Path,
    ak_pub_file: &Path,
) -> anyhow::Result<std::collections::HashMap<u32, String>> {
    let output = Command::new("tpm2_checkquote")
        .arg("--public")
        .arg(ak_pub_file)
        .arg("--message")
        .arg(msg_file)
        .arg("--pcr")
        .arg(pcr_file)
        .arg("--signature")
        .arg(sig_file)
        .output()
        .context("failed to execute tpm2_checkquote")?;

    if !output.status.success() {
        bail!(
            "tpm2_checkquote failed (exit {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    tracing::debug!("tpm2_checkquote output:\n{}", stdout);

    // Parse the output produced by tpm2_checkquote
    let mut pcr_map = std::collections::HashMap::new();
    let mut in_sha256 = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("sha256:") {
            in_sha256 = true;
            continue;
        }
        if in_sha256 {
            // Any line that starts a new non-indented key ends the sha256 block.
            if !line.starts_with(' ') && !line.starts_with('\t') {
                in_sha256 = false;
                continue;
            }
            // Expected format: "  <index> : <0xdigest>"
            if let Some((idx_str, digest_str)) = trimmed.split_once(':') {
                if let Ok(idx) = idx_str.trim().parse::<u32>() {
                    pcr_map.insert(idx, digest_str.trim().to_string());
                }
            }
        }
    }

    if pcr_map.is_empty() {
        bail!("tpm2_checkquote output contained no PCR digests");
    }

    Ok(pcr_map)
}

pub fn check_policy(claims: &MaaClaims, policy: &Value) -> anyhow::Result<()> {
    let payload = serde_json::to_value(claims).context("failed to serialize claims to JSON")?;

    let map: [(&str, &str); 8] = [
        (
            "attestation-type",
            "/x-ms-isolation-tee/x-ms-attestation-type",
        ),
        (
            "compliance-status",
            "/x-ms-isolation-tee/x-ms-compliance-status",
        ),
        ("secureboot", "/secureboot"),
        ("kerneldebug-enabled", "/x-ms-azurevm-kerneldebug-enabled"),
        ("imageId", "/x-ms-isolation-tee/x-ms-sevsnpvm-imageId"),
        (
            "microcode-svn",
            "/x-ms-isolation-tee/x-ms-sevsnpvm-microcode-svn",
        ),
        ("snpfw-svn", "/x-ms-isolation-tee/x-ms-sevsnpvm-snpfw-svn"),
        (
            "launch_measurement",
            "/x-ms-isolation-tee/x-ms-sevsnpvm-launchmeasurement",
        ),
    ];

    for (key, ptr) in map {
        let expected = policy
            .get(key)
            .with_context(|| format!("policy missing key {}", key))?;

        let actual = payload
            .pointer(ptr)
            .with_context(|| format!("attestation missing {}", ptr))?;

        if actual == expected {
            eprintln!("Policy check passed for {}: {}", key, actual);
        } else {
            bail!(
                "policy mismatch @ {}\n  expected: {}\n  got     : {}",
                key,
                expected,
                actual
            );
        }
    }

    eprintln!("Attestation compliant with the policy!");
    Ok(())
}

async fn verify_attestation_response(
    resp: ResponseStruct,
    dapp_attestation_url: &str,
    policy: &Value,
) -> anyhow::Result<()> {
    let static_attestation_doc = fetch_static_attestation_document(dapp_attestation_url).await?;
    let key = ring::aead::LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &resp.key).unwrap());
    let mut data = resp.maa_resp.token.jwt.clone();
    let nonce =
        Nonce::try_assume_unique_for_key(&resp.maa_resp.token.encryption_params.iv).unwrap();
    let tag = Tag::try_from(&resp.maa_resp.token.authentication_data[..]).unwrap();
    key.open_in_place_separate_tag(
        nonce,
        Aad::from("Transport Key".as_bytes()),
        tag,
        &mut data,
        0..,
    )
    .unwrap();

    let jwt = String::from_utf8(data)?;

    let mut client_builder = reqwest::Client::builder().use_rustls_tls();
    
    // Create trust anchors based on Root CAs used by Azure. The certs are embedded at compile time.
    // The certificates were taken from here:
    // https://learn.microsoft.com/en-us/azure/security/fundamentals/azure-ca-details?tabs=root-and-subordinate-cas-list

    let azure_allowed_root_ca = [
        include_bytes!("ca_azure/Microsoft RSA Root Certificate Authority 2017.crt").to_vec(),
        include_bytes!("ca_azure/Microsoft ECC Root Certificate Authority 2017.crt").to_vec(),
        include_bytes!("ca_azure/BaltimoreCyberTrustRoot.crt").to_vec(),
        include_bytes!("ca_azure/DigiCertGlobalRootCA.crt").to_vec(),
        include_bytes!("ca_azure/DigiCertGlobalRootG2.crt").to_vec(),
        include_bytes!("ca_azure/DigiCertGlobalRootG3.crt").to_vec(),
    ];

    let mut certs = vec![];

    for cert in azure_allowed_root_ca.iter() {
        certs.push(reqwest::Certificate::from_der(cert.as_slice())?);
    }

    client_builder = client_builder.tls_built_in_root_certs(false);

    for cert in certs {
        client_builder = client_builder.add_root_certificate(cert);
    }

    let client = client_builder.build()?;

    let jwks: JwkSet = client
        .get(MAA_ATTESTATION_URL)
        .send()
        .await?
        .json()
        .await?;

    let header = jsonwebtoken::decode_header(&jwt).unwrap();

    let Some(kid) = header.kid else {
        bail!("JWT doesn't have a `kid` header field");
    };

    let Some(jwk) = jwks.find(&kid) else {
        bail!("No matching JWK found for the given kid");
    };

    let validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    let maa_token: TokenData<MaaClaims> =
        jsonwebtoken::decode(&jwt, &DecodingKey::from_jwk(jwk)?, &validation)?;

    if maa_token.claims.iss != MAA_ATTESTATION_URL
        || maa_token.claims.x_ms_attestation_type != "azurevm"
        || maa_token.claims.x_ms_isolation_tee.x_ms_attestation_type != "sevsnpvm"
        || maa_token.claims.x_ms_isolation_tee.x_ms_compliance_status != "azure-compliant-cvm"
        || maa_token
            .claims
            .x_ms_isolation_tee
            .x_ms_sevsnpvm_is_debuggable
    {
        bail!("MAA claims are unsecure, verification failed");
    }

    println!(
        "{}",
        "Claims coming from a compliant Azure VM, verification successful"
    );
    println!("Now verifying the measurements and PCRs using the TPM Quote...");
    check_policy(&maa_token.claims, policy)?;

    println!("{:#?}", maa_token);

    println!("Decoding Message...");

    let msg = BASE64_STANDARD.decode(resp.msg)?;
    let mut msg_file = NamedTempFile::new()?;
    msg_file.write_all(&msg)?;

    let sig = BASE64_STANDARD.decode(resp.sig)?;
    let mut sig_file = NamedTempFile::new()?;
    sig_file.write_all(&sig)?;

    let pcr = BASE64_STANDARD.decode(resp.pcr)?;
    let mut pcr_file = NamedTempFile::new()?;
    pcr_file.write_all(&pcr)?;

    let ak_pub_list = maa_token.claims.x_ms_isolation_tee.x_ms_runtime.keys;

    let mut verified_quote = false;
    for key in ak_pub_list {
        if key.kid == "HCLAkPub" {
            let ak_pub_key_n = URL_SAFE_NO_PAD.decode(key.n)?;
            let ak_pub_key_e = URL_SAFE_NO_PAD.decode(key.e)?;
            let ak_pub_key_bignum_n = BigNum::from_slice(&ak_pub_key_n)?;
            let ak_pub_key_bignum_e = BigNum::from_slice(&ak_pub_key_e)?;
            let rsa_pub =
                rsa::Rsa::from_public_components(ak_pub_key_bignum_n, ak_pub_key_bignum_e)?;

            let mut ak_pub_file = NamedTempFile::new()?;
            ak_pub_file.write_all(&rsa_pub.public_key_to_pem()?)?;
            tracing::info!("Verifying TPM Quote...");
            let pcr_map = verify_measurements(
                msg_file.path(),
                sig_file.path(),
                pcr_file.path(),
                ak_pub_file.path(),
            )?;

            // Now that we cryptographically verified the TPM quote, we will compare the actual
            // PCR digests parsed from tpm2_checkquote against the expected values in
            // the PCR policy
            let expected_measurements = policy
                .get("measurements")
                .context("policy missing \"measurements\" key")?;
            verify_pcr_values(&pcr_map, expected_measurements)?;
            verify_application_pcr15(&pcr_map, policy, &static_attestation_doc)?;
            verified_quote = true;
        }
    }

    if !verified_quote {
        bail!("MAA token did not contain the HCLAkPub key needed to verify the TPM quote");
    }

    Ok(())
}

pub async fn verify_attestation_payload(
    attestation: Value,
    dapp_attestation_url: &str,
    policy: &Value,
) -> anyhow::Result<()> {
    let resp: ResponseStruct =
        serde_json::from_value(attestation).context("invalid attestation payload")?;
    verify_attestation_response(resp, dapp_attestation_url, policy).await
}

pub async fn get_attestation(url: &str, policy: &Value) -> anyhow::Result<()> {
    let client = Client::new();
    let resp: ResponseStruct = client.get(url).send().await?.json().await?;
    verify_attestation_response(resp, url, policy).await
}

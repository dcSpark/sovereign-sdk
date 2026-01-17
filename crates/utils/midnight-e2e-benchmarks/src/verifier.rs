use anyhow::Context;
use serde::Serialize;
use sov_modules_api::PublicKey;
use sov_proof_verifier_service::{create_router, AppState, RollupSpec, ServiceConfig};
use tokio::net::TcpListener;

/// Start a local proof verifier service bound to an ephemeral port.
pub async fn start_local_verifier(
    api_url: &str,
    method_id: [u8; 32],
    da_connection_string: &str,
    max_concurrent_verifications: usize,
    defer_sequencer_submission: bool,
) -> anyhow::Result<String> {
    use sov_rollup_interface::crypto::PrivateKey as _;

    let sk: <<RollupSpec as sov_modules_api::Spec>::CryptoSpec as sov_rollup_interface::zk::CryptoSpec>::PrivateKey =
        <<RollupSpec as sov_modules_api::Spec>::CryptoSpec as sov_rollup_interface::zk::CryptoSpec>::PrivateKey::generate();
    let pk = sk.pub_key();
    let addr: <RollupSpec as sov_modules_api::Spec>::Address = pk.credential_id().into();

    #[derive(Serialize)]
    struct KeyFile<'a, PK> {
        private_key: &'a PK,
        address: <RollupSpec as sov_modules_api::Spec>::Address,
    }

    let tmpkey = tempfile::NamedTempFile::new().context("Failed to create temp key file")?;
    std::fs::write(
        tmpkey.path(),
        serde_json::to_string(&KeyFile {
            private_key: &sk,
            address: addr,
        })?,
    )?;

    let verifier_cfg = ServiceConfig {
        node_rpc_url: api_url.to_string(),
        signing_key_path: tmpkey.path().to_string_lossy().to_string(),
        value_setter_method_id: None,
        midnight_method_id: Some(method_id),
        max_concurrent_verifications,
        chain_id: 1,
        da_connection_string: da_connection_string.to_string(),
        defer_sequencer_submission,
        prover_service_url: None, // Use local daemon pool for benchmarks
    };

    let state = AppState::new(verifier_cfg)
        .await
        .context("Failed to create AppState")?;
    let app = create_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let service_addr = listener.local_addr()?;
    let verifier_url = format!("http://{}", service_addr);
    tokio::spawn(async move {
        axum::serve(
            listener,
            axum::ServiceExt::<axum::extract::Request>::into_make_service(app),
        )
        .await
        .expect("Failed to serve proof verifier service");
    });
    std::mem::forget(tmpkey);

    let hc = reqwest::Client::new();
    let _ = hc.get(format!("{}/health", verifier_url)).send().await;

    Ok(verifier_url)
}

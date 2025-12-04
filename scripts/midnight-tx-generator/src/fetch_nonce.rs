use anyhow::Result;
use serde::Deserialize;
use sov_cli::wallet_state::PrivateKeyAndAddress;
use sov_modules_api::execution_mode::Native;
use sov_modules_api::PrivateKey; // bring trait into scope for pub_key()
use sov_modules_rollup_blueprint::RollupBlueprint;
use sov_rollup_interface::crypto::PublicKey as _; // for credential_id()
use std::env;
use std::fs;

type DemoRollupSpec = <sov_rollup_ligero::MockDemoRollup<Native> as RollupBlueprint<Native>>::Spec;

fn usage() -> ! {
    eprintln!("Usage: fetch-nonce <node_api_url> <private_key_file>");
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let node_url = args.next().unwrap_or_else(|| usage());
    let key_path = args.next().unwrap_or_else(|| usage());

    let key_json = fs::read_to_string(&key_path)?;
    let key_data: PrivateKeyAndAddress<DemoRollupSpec> = serde_json::from_str(&key_json)?;

    // Build dedup URL for generation
    let pub_key = key_data.private_key.pub_key();
    let cred = pub_key.credential_id();
    let url = format!(
        "{}/rollup/addresses/{}/dedup?select=generation",
        node_url, cred
    );

    #[derive(Deserialize)]
    struct DedupResponse {
        #[allow(dead_code)]
        nonce: Option<u64>,
        generation: Option<u64>,
    }

    let resp = reqwest::get(url).await;
    let next_gen = match resp {
        Ok(r) => match r.json::<DedupResponse>().await {
            Ok(d) => d.generation.unwrap_or(0),
            Err(_) => 0,
        },
        Err(_) => 0,
    };

    println!("{}", next_gen);
    Ok(())
}

use anyhow::Result;
use midnight_e2e_benchmarks::continuous_transfers;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    continuous_transfers::run().await
}

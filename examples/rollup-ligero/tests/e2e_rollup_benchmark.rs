use anyhow::Result;

#[tokio::test(flavor = "multi_thread")]
async fn e2e_rollup_benchmark() -> Result<()> {
    let config = sov_rollup_ligero::e2e_runner::RunnerConfig::from_env();
    sov_rollup_ligero::e2e_runner::run(config).await
}

use anyhow::Result;

#[tokio::test(flavor = "multi_thread")]
async fn e2e_rollup_benchmark() -> Result<()> {
    let config = midnight_e2e_benchmarks::e2e_runner::RunnerConfig::from_env();
    midnight_e2e_benchmarks::e2e_runner::run(config).await
}

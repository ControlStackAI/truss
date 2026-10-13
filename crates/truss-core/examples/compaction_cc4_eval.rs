use truss_core::compaction_eval::run_default_eval;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let report = run_default_eval().await?;
    println!("{}", report.pretty_table());
    println!("Momentum proxy: {}", report.momentum_proxy);
    println!("Limitations: {}", report.limitations);
    Ok(())
}

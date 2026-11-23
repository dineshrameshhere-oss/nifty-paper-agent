use anyhow::Result;
use nifty_paper_agent::{config::Settings, journal::Journal, strategy::{IndependentStrategy, NoopStrategy}, LIVE_ORDER_PLACEMENT_IMPLEMENTED};
use std::sync::Arc;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    dotenvy::dotenv().ok();
    let settings = Settings::load("settings.toml")?;
    if LIVE_ORDER_PLACEMENT_IMPLEMENTED { anyhow::bail!("live order placement must remain disabled") }
    std::fs::create_dir_all("data")?;
    let _journal = Journal::open("data/paper.sqlite")?;
    let strategy: Arc<dyn IndependentStrategy> = Arc::new(NoopStrategy);
    info!(capital_rupees = settings.paper.starting_capital_paise / 100, workers=settings.analysis.worker_count,
        subleaders=settings.analysis.subleader_count, driver=settings.analysis.driver_count,
        mode=%settings.analysis.mode, "paper-only NIFTY agent ready");
    if settings.analysis.worker_count != 10 || settings.analysis.subleader_count != 2 || settings.analysis.driver_count != 1 {
        anyhow::bail!("starter orchestrator expects 10 workers, 2 subleaders, 1 Driver")
    }
    if settings.indstocks.market_data_enabled {
        warn!("adapter enabled, but endpoint and schema must be verified against current INDstocks documentation before use")
    } else {
        info!("market-data adapter disabled; no network requests made. Strategy adapter is an independent no-op placeholder.");
    }
    let _ = strategy;
    info!("exiting safely; no live order APIs exist in this project");
    Ok(())
}

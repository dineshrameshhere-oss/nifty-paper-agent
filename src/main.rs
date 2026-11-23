use anyhow::Result;
use nifty_paper_agent::{
    agents::GeminiOrchestrator,
    backtest::{fetch_yahoo_nifty_5m, load_index_history, run_agent_backtest},
    config::Settings,
    journal::Journal,
    strategy::{IndependentStrategy, NoopStrategy},
    LIVE_ORDER_PLACEMENT_IMPLEMENTED,
};
use std::sync::Arc;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    dotenvy::dotenv().ok();
    let settings = Settings::load("settings.toml")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "backtest") {
        if LIVE_ORDER_PLACEMENT_IMPLEMENTED {
            anyhow::bail!("live order placement must remain disabled")
        }
        let data_path = args
            .windows(2)
            .find(|pair| pair[0] == "--data")
            .map(|p| p[1].as_str())
            .unwrap_or("data/yahoo_nifty50_60d_5m.json");
        let max_decisions = args
            .windows(2)
            .find(|pair| pair[0] == "--max-decisions")
            .and_then(|p| p[1].parse::<usize>().ok())
            .unwrap_or(1);
        let bars = if std::path::Path::new(data_path).exists() {
            load_index_history(data_path)?
        } else {
            fetch_yahoo_nifty_5m(data_path).await?
        };
        let key = std::env::var("GEMINI_API_KEY").map_err(|_| {
            anyhow::anyhow!(
                "set GEMINI_API_KEY with scripts/update-gemini-key.sh before backtesting"
            )
        })?;
        let policy = GeminiOrchestrator::new(settings.llm.clone(), key);
        let synthetic_config = nifty_paper_agent::backtest::config_for_bars(&bars);
        let report = run_agent_backtest(
            &bars,
            &policy,
            &settings.paper,
            &synthetic_config,
            max_decisions,
        )
        .await?;
        std::fs::create_dir_all("data")?;
        std::fs::write(
            "data/backtest-result.json",
            serde_json::to_vec_pretty(&report)?,
        )?;
        println!("NIFTY synthetic-options paper backtest: decisions={}, trades={}, net P&L=₹{:.2}, max drawdown=₹{:.2}; report=data/backtest-result.json",
            report.decisions,report.trades.len(),report.net_pnl_rupees,report.max_drawdown_rupees);
        println!("Prices are synthetic Black-Scholes estimates. This did not place live orders.");
        return Ok(());
    }
    if LIVE_ORDER_PLACEMENT_IMPLEMENTED {
        anyhow::bail!("live order placement must remain disabled")
    }
    std::fs::create_dir_all("data")?;
    let _journal = Journal::open("data/paper.sqlite")?;
    let strategy: Arc<dyn IndependentStrategy> = Arc::new(NoopStrategy);
    info!(capital_rupees = settings.paper.starting_capital_paise / 100, workers=settings.analysis.worker_count,
        subleaders=settings.analysis.subleader_count, driver=settings.analysis.driver_count,
        mode=%settings.analysis.mode, "paper-only NIFTY agent ready");
    if settings.analysis.worker_count != 10
        || settings.analysis.subleader_count != 2
        || settings.analysis.driver_count != 1
    {
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

use anyhow::Result;
use chrono::Utc;
use nifty_paper_agent::{
    agents::TradingAgentsOrchestrator,
    backtest::{fetch_yahoo_nifty_5m, load_index_history, run_agent_backtest},
    config::Settings,
    data::IndstocksClient,
    journal::Journal,
    paper_runner::PaperRunner,
    strategy::{IndependentStrategy, NoopStrategy},
    types::{MarketSnapshot, Quote},
    LIVE_ORDER_PLACEMENT_IMPLEMENTED,
};
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    dotenvy::dotenv().ok();
    let mut settings = Settings::load("settings.toml")?;
    if LIVE_ORDER_PLACEMENT_IMPLEMENTED {
        anyhow::bail!("live order placement must remain disabled")
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let offline = args.iter().any(|a| a == "--offline");
    if offline {
        settings.llm.use_offline_deterministic = true;
    }

    if args.first().is_some_and(|arg| arg == "backtest") {
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
        std::fs::create_dir_all("data")?;
        let journal = Arc::new(Journal::open("data/paper.sqlite")?);
        let policy = TradingAgentsOrchestrator::from_settings(&settings)?.with_journal(journal);
        let synthetic_config = nifty_paper_agent::backtest::config_for_bars(&bars);
        let report = run_agent_backtest(
            &bars,
            &policy,
            &settings.paper,
            &synthetic_config,
            max_decisions,
        )
        .await?;
        std::fs::write(
            "data/backtest-result.json",
            serde_json::to_vec_pretty(&report)?,
        )?;
        println!(
            "NIFTY synthetic-options paper backtest: decisions={}, trades={}, net P&L=₹{:.2}, max drawdown=₹{:.2}; report=data/backtest-result.json",
            report.decisions,
            report.trades.len(),
            report.net_pnl_rupees,
            report.max_drawdown_rupees
        );
        println!("Prices are synthetic Black-Scholes estimates. This did not place live orders.");
        return Ok(());
    }

    std::fs::create_dir_all("data")?;
    let journal = Arc::new(Journal::open("data/paper.sqlite")?);
    let strategy: Arc<dyn IndependentStrategy> = Arc::new(NoopStrategy);

    if args.first().is_some_and(|arg| arg == "paper") {
        let demo = args.iter().any(|a| a == "--demo");
        let orchestrator = TradingAgentsOrchestrator::from_settings(&settings)?;
        let mut runner =
            PaperRunner::new(settings.clone(), orchestrator, journal)?.with_strategy(strategy);
        if demo {
            let now = Utc::now();
            let snapshot = MarketSnapshot {
                id: Uuid::new_v4(),
                created_at: now,
                quotes: vec![Quote {
                    symbol: format!("{}-DEMO-CE", settings.indstocks.symbol),
                    bid: 100.0,
                    ask: 101.0,
                    last: 100.5,
                    timestamp: now,
                }],
                context: "Paper demo snapshot; spot=25350".into(),
            };
            let res = runner.step(&snapshot).await?;
            info!(
                approved = res.trace.risk_verdict.approved,
                filled = res.fill.is_some(),
                "completed paper demo step"
            );
            return Ok(());
        }
        if !settings.indstocks.market_data_enabled {
            info!("market_data_enabled=false; pass --demo to run a synthetic paper step");
            return Ok(());
        }
        let token = std::env::var("INDSTOCKS_ACCESS_TOKEN").unwrap_or_default();
        let client = IndstocksClient::new(settings.indstocks.clone(), token);
        let snapshot = client.snapshot().await?;
        let res = runner.step(&snapshot).await?;
        info!(
            approved = res.trace.risk_verdict.approved,
            filled = res.fill.is_some(),
            "completed live-feed paper step"
        );
        return Ok(());
    }

    info!(
        capital_rupees = settings.paper.starting_capital_paise / 100,
        debate_rounds = settings.analysis.max_debate_rounds,
        risk_rounds = settings.analysis.max_risk_discuss_rounds,
        reflection_enabled = settings.analysis.reflection_enabled,
        mode = %settings.analysis.mode,
        "paper-only TradingAgents NIFTY agent ready"
    );
    if settings.indstocks.market_data_enabled {
        warn!("adapter enabled, but endpoint and schema must be verified against current INDstocks documentation before use");
    } else {
        info!("market-data adapter disabled; no network requests made. Use `backtest` or `paper --demo` to run.");
    }
    let _ = (journal, strategy);
    info!("exiting safely; no live order APIs exist in this project");
    Ok(())
}

use anyhow::Result;
use chrono::Utc;
use nifty_paper_agent::{
    agents::TradingAgentsOrchestrator,
    backtest::{fetch_yahoo_nifty_5m, load_index_history, run_agent_backtest},
    clock::SystemClock,
    config::Settings,
    data::IndstocksClient,
    journal::Journal,
    paper_runner::{run_paper_session, PaperRunner},
    strategy::{IndependentStrategy, NoopStrategy},
    types::{MarketSnapshot, Quote},
    LIVE_ORDER_PLACEMENT_IMPLEMENTED,
};
use std::sync::Arc;
use tracing::info;
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq)]
enum CliCommand<'a> {
    Backtest {
        data_path: &'a str,
        max_decisions: usize,
    },
    Paper {
        demo: bool,
    },
    Status,
}

fn parse_cli_command(args: &[String]) -> (bool, CliCommand<'_>) {
    let offline = args.iter().any(|a| a == "--offline");
    let cmd = match args.first().map(String::as_str) {
        Some("backtest") => {
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
            CliCommand::Backtest {
                data_path,
                max_decisions,
            }
        }
        Some("paper") => {
            let demo = args.iter().any(|a| a == "--demo");
            CliCommand::Paper { demo }
        }
        _ => CliCommand::Status,
    };
    (offline, cmd)
}

fn validate_live_paper_prereqs(settings: &Settings, token: &str) -> Result<()> {
    if !settings.indstocks.market_data_enabled {
        anyhow::bail!(
            "indstocks.market_data_enabled is false in settings.toml; set it to true for live-data paper trading or pass --demo"
        );
    }
    let trimmed = token.trim();
    if trimmed.is_empty() || trimmed.contains(['\r', '\n']) || trimmed.contains("REPLACE_WITH") {
        anyhow::bail!(
            "INDSTOCKS_ACCESS_TOKEN is missing or invalid; update .env via scripts/update-indstocks-token.sh (Termux/Unix) or scripts/update-indstocks-token.ps1 (Windows), or export INDSTOCKS_ACCESS_TOKEN"
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_ansi(true)
        .init();
    dotenvy::dotenv().ok();
    let mut settings = Settings::load("settings.toml")?;
    if LIVE_ORDER_PLACEMENT_IMPLEMENTED {
        anyhow::bail!("live order placement must remain disabled")
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let (offline, cmd) = parse_cli_command(&args);
    if offline {
        settings.llm.use_offline_deterministic = true;
    }

    match cmd {
        CliCommand::Backtest {
            data_path,
            max_decisions,
        } => {
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
        CliCommand::Paper { demo } => {
            std::fs::create_dir_all("data")?;
            let journal = Arc::new(Journal::open("data/paper.sqlite")?);
            let strategy: Arc<dyn IndependentStrategy> = Arc::new(NoopStrategy);

            if demo {
                let orchestrator = TradingAgentsOrchestrator::from_settings(&settings)?;
                let mut runner = PaperRunner::new(settings.clone(), orchestrator, journal)?
                    .with_strategy(strategy);
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

            let token = std::env::var("INDSTOCKS_ACCESS_TOKEN").unwrap_or_default();
            validate_live_paper_prereqs(&settings, &token)?;

            let orchestrator = TradingAgentsOrchestrator::from_settings(&settings)?;
            let mut runner =
                PaperRunner::new(settings.clone(), orchestrator, journal)?.with_strategy(strategy);
            let client = IndstocksClient::new(settings.indstocks.clone(), token);
            let clock = SystemClock;
            let summary = run_paper_session(
                &mut runner,
                &client,
                &clock,
                tokio::time::sleep,
                async {
                    let _ = tokio::signal::ctrl_c().await;
                },
            )
            .await?;
            println!("{summary}");
            return Ok(());
        }
        CliCommand::Status => {
            std::fs::create_dir_all("data")?;
            let journal = Arc::new(Journal::open("data/paper.sqlite")?);
            let strategy: Arc<dyn IndependentStrategy> = Arc::new(NoopStrategy);

            info!(
                capital_rupees = settings.paper.starting_capital_paise / 100,
                debate_rounds = settings.analysis.max_debate_rounds,
                risk_rounds = settings.analysis.max_risk_discuss_rounds,
                reflection_enabled = settings.analysis.reflection_enabled,
                mode = %settings.analysis.mode,
                "paper-only TradingAgents NIFTY agent ready"
            );
            if settings.indstocks.market_data_enabled {
                info!("INDstocks live market-data adapter enabled; run `paper` for continuous NSE market-hours session (09:15-15:30 IST), `paper --demo`, or `backtest`.");
            } else {
                info!("market-data adapter disabled; no network requests made. Use `backtest` or `paper --demo` to run.");
            }
            let _ = (journal, strategy);
            info!("exiting safely; no live order APIs exist in this project");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_live_paper_prereqs_rejects_disabled_and_invalid_tokens() {
        let mut settings = Settings::default();
        settings.indstocks.market_data_enabled = false;
        let err = validate_live_paper_prereqs(&settings, "valid-token").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("market_data_enabled"));
        assert!(msg.contains("--demo"));

        settings.indstocks.market_data_enabled = true;
        for bad in [
            "",
            "   ",
            "tok\nnext",
            "tok\r\nnext",
            "REPLACE_WITH_TOKEN",
        ] {
            let err = validate_live_paper_prereqs(&settings, bad).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("INDSTOCKS_ACCESS_TOKEN"), "bad={bad:?} msg={msg}");
            assert!(msg.contains("scripts/update-indstocks-token.sh"));
            assert!(msg.contains("scripts/update-indstocks-token.ps1"));
        }

        assert!(validate_live_paper_prereqs(&settings, "live-daily-token-123").is_ok());
    }

    #[test]
    fn parse_cli_command_supports_paper_demo_offline_and_backtest() {
        let args = vec![
            "paper".to_string(),
            "--demo".to_string(),
            "--offline".to_string(),
        ];
        let (offline, cmd) = parse_cli_command(&args);
        assert!(offline);
        assert_eq!(cmd, CliCommand::Paper { demo: true });

        let live_args = vec!["paper".to_string()];
        let (offline_live, cmd_live) = parse_cli_command(&live_args);
        assert!(!offline_live);
        assert_eq!(cmd_live, CliCommand::Paper { demo: false });

        let bt_args = vec![
            "backtest".to_string(),
            "--data".to_string(),
            "custom.json".to_string(),
            "--max-decisions".to_string(),
            "8".to_string(),
            "--offline".to_string(),
        ];
        let (offline_bt, cmd_bt) = parse_cli_command(&bt_args);
        assert!(offline_bt);
        assert_eq!(
            cmd_bt,
            CliCommand::Backtest {
                data_path: "custom.json",
                max_decisions: 8
            }
        );

        let empty: Vec<String> = Vec::new();
        let (offline_empty, cmd_empty) = parse_cli_command(&empty);
        assert!(!offline_empty);
        assert_eq!(cmd_empty, CliCommand::Status);
    }
}

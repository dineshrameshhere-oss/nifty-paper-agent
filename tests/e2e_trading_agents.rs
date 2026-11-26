use chrono::{Duration, TimeZone, Utc};
use nifty_paper_agent::{
    agents::TradingAgentsOrchestrator,
    backtest::{run_agent_backtest, IndexBar, SyntheticConfig},
    config::Settings,
    journal::Journal,
    paper_runner::PaperRunner,
    types::{MarketSnapshot, Quote, Side},
    LIVE_ORDER_PLACEMENT_IMPLEMENTED,
};
use std::sync::Arc;
use uuid::Uuid;

#[tokio::test]
async fn e2e_paper_trading_round_trip_with_reflection_and_memory_recall() {
    assert!(!LIVE_ORDER_PLACEMENT_IMPLEMENTED);

    let settings = Settings::load("settings.toml").unwrap();
    let db = tempfile::NamedTempFile::new().unwrap();
    let journal = Arc::new(Journal::open(db.path().to_str().unwrap()).unwrap());
    let orch =
        TradingAgentsOrchestrator::deterministic(settings.analysis.clone(), settings.llm.clone())
            .with_journal(journal.clone());
    let mut runner = PaperRunner::new(settings, orch, journal.clone()).unwrap();

    // Step 1: Flat -> Deterministic TradingAgents opens 1-lot long position
    let now = Utc::now();
    let snap_buy = MarketSnapshot {
        id: Uuid::new_v4(),
        created_at: now,
        quotes: vec![Quote {
            symbol: "NIFTY-2026-11-24-25350-CE".into(),
            bid: 100.0,
            ask: 101.0,
            last: 100.5,
            timestamp: now,
        }],
        context: "Paper session bar 1; nifty options bullish".into(),
    };
    let res1 = runner.step(&snap_buy).await.unwrap();
    assert!(res1.trace.risk_verdict.approved);
    let fill1 = res1.fill.expect("expected buy fill");
    assert_eq!(fill1.side, Side::Buy);
    assert_eq!(runner.broker.open_positions_count(), 1);
    assert_eq!(journal.signal_count().unwrap(), 1);
    assert_eq!(journal.fill_count().unwrap(), 1);

    // Step 2: Price hits target -> Deterministic TradingAgents exits & records reflection
    let now2 = Utc::now();
    let snap_exit = MarketSnapshot {
        id: Uuid::new_v4(),
        created_at: now2,
        quotes: vec![Quote {
            symbol: "NIFTY-2026-11-24-25350-CE".into(),
            bid: 150.0,
            ask: 151.0,
            last: 150.5,
            timestamp: now2,
        }],
        context: "Paper session bar 2; target reached".into(),
    };
    let res2 = runner.step(&snap_exit).await.unwrap();
    let fill2 = res2.fill.expect("expected sell exit fill");
    assert_eq!(fill2.side, Side::Sell);
    assert!(res2.reflection_recorded);
    assert_eq!(runner.broker.open_positions_count(), 0);
    assert!(runner.broker.realized_pnl_paise() > 0);
    assert_eq!(journal.reflection_count().unwrap(), 1);

    // Step 3: Next cycle recalls the stored reflection memory
    let now3 = Utc::now();
    let snap_next = MarketSnapshot {
        id: Uuid::new_v4(),
        created_at: now3,
        quotes: vec![Quote {
            symbol: "NIFTY-2026-11-24-25350-CE".into(),
            bid: 110.0,
            ask: 111.0,
            last: 110.5,
            timestamp: now3,
        }],
        context: "Paper session bar 3; nifty options".into(),
    };
    let res3 = runner.step(&snap_next).await.unwrap();
    assert!(!res3.trace.retrieved_memories.is_empty());
}

#[tokio::test]
async fn e2e_synthetic_backtest_with_trading_agents_and_reflection_persistence() {
    let mut settings = Settings::load("settings.toml").unwrap();
    settings.paper.max_trade_value_paise = 2_000_000;

    let db = tempfile::NamedTempFile::new().unwrap();
    let journal = Arc::new(Journal::open(db.path().to_str().unwrap()).unwrap());
    let policy =
        TradingAgentsOrchestrator::deterministic(settings.analysis.clone(), settings.llm.clone())
            .with_journal(journal.clone());

    let start = Utc.with_ymd_and_hms(2026, 11, 20, 4, 0, 0).unwrap();
    let bars: Vec<IndexBar> = (0..5)
        .map(|i| IndexBar {
            timestamp: start + Duration::minutes(i * 5),
            open: 25300.0 + i as f64 * 20.0,
            high: 25325.0 + i as f64 * 20.0,
            low: 25285.0 + i as f64 * 20.0,
            close: 25315.0 + i as f64 * 20.0,
            volume: 1000,
        })
        .collect();

    let report = run_agent_backtest(
        &bars,
        &policy,
        &settings.paper,
        &SyntheticConfig::default(),
        4,
    )
    .await
    .unwrap();

    assert_eq!(report.decisions, 4);
    assert!(!report.trades.is_empty());
    assert!(journal.reflection_count().unwrap() >= 1);
}

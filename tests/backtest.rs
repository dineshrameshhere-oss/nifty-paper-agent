use anyhow::Result;
use chrono::{TimeZone, Utc};
use nifty_paper_agent::{
    backtest::{
        load_yahoo_chart, rolling_volatility, run_agent_backtest, synthetic_chain, BacktestPolicy,
        BacktestPosition, IndexBar, OptionRight, SyntheticConfig,
    },
    config::Settings,
    types::{MarketSnapshot, Side, Signal},
};
use std::sync::Mutex;

#[test]
fn reads_yahoo_chart_json_and_skips_null_bars() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(),r#"{"chart":{"result":[{"timestamp":[1,2,3],"indicators":{"quote":[{"open":[100,null,99],"high":[101,null,100],"low":[99,null,98],"close":[100.5,null,99.5],"volume":[10,null,12]}]}}]}}"#).unwrap();
    let bars = load_yahoo_chart(file.path()).unwrap();
    assert_eq!(bars.len(), 2);
    assert_eq!(bars[0].close, 100.5);
}

#[test]
fn reads_indmoney_ohlc_json_as_ist_wall_clock() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(),r#"{"candles":[{"datetime_ist":"2026-11-20 09:15:00","open":100,"high":102,"low":99,"close":101,"volume":10},{"datetime_ist":"2026-11-20 09:20:00","open":101,"high":103,"low":100,"close":102,"volume":11}]}"#).unwrap();
    let bars = nifty_paper_agent::backtest::load_indmoney_ohlc(file.path()).unwrap();
    assert_eq!(
        bars[0].timestamp,
        Utc.with_ymd_and_hms(2026, 11, 20, 3, 45, 0).unwrap()
    );
}

#[test]
fn synthetic_chain_has_dynamic_atm_strikes_and_two_sides() {
    let bar = IndexBar {
        timestamp: Utc.with_ymd_and_hms(2026, 11, 23, 10, 0, 0).unwrap(),
        open: 25300.0,
        high: 25400.0,
        low: 25250.0,
        close: 25367.0,
        volume: 100,
    };
    let chain = synthetic_chain(&bar, 0.20, &SyntheticConfig::default()).unwrap();
    assert_eq!(chain.len(), 22);
    assert!(chain
        .iter()
        .any(|q| q.strike == 25350 && q.right == OptionRight::Call));
    assert!(chain.iter().all(|q| q.bid > 0.0
        && q.ask >= q.bid
        && q.expiry == chrono::NaiveDate::from_ymd_opt(2026, 11, 24).unwrap()));
}

#[test]
fn rolling_volatility_is_bounded_and_uses_fallback_for_short_history() {
    assert_eq!(rolling_volatility(&[100.0, 101.0], 18900.0, 0.22), 0.22);
    let vol = rolling_volatility(&[100.0, 101.0, 99.0, 102.0, 100.0], 18900.0, 0.22);
    assert!((0.08..=1.5).contains(&vol));
}

struct RoundTripPolicy {
    symbol: Mutex<Option<String>>,
}
#[async_trait::async_trait]
impl BacktestPolicy for RoundTripPolicy {
    async fn decide(
        &self,
        snapshot: &MarketSnapshot,
        position: Option<&BacktestPosition>,
    ) -> Result<Option<Signal>> {
        let (symbol, side) = if let Some(p) = position {
            (p.symbol.clone(), Side::Sell)
        } else {
            let q = snapshot
                .quotes
                .iter()
                .filter(|q| q.symbol.ends_with("-PE"))
                .min_by(|a, b| a.ask.total_cmp(&b.ask))
                .unwrap();
            *self.symbol.lock().unwrap() = Some(q.symbol.clone());
            (q.symbol.clone(), Side::Buy)
        };
        Ok(Some(Signal {
            id: format!("{}-{}", symbol, snapshot.created_at.timestamp()),
            source: "test".into(),
            symbol,
            side,
            quantity: 1,
            limit_price: 10000.0,
            confidence_bps: 9000,
            rationale: "test".into(),
            snapshot_id: Some(snapshot.id),
        }))
    }
}

#[tokio::test]
async fn replays_agent_entry_and_exit_with_next_bar_fills() {
    let start = Utc.with_ymd_and_hms(2026, 11, 20, 10, 0, 0).unwrap();
    let bars = (0..3)
        .map(|i| IndexBar {
            timestamp: start + chrono::Duration::minutes(i * 5),
            open: 25300.0 + i as f64 * 25.0,
            high: 25325.0 + i as f64 * 25.0,
            low: 25275.0 + i as f64 * 25.0,
            close: 25310.0 + i as f64 * 25.0,
            volume: 100,
        })
        .collect::<Vec<_>>();
    let settings = Settings::load("settings.toml").unwrap();
    let mut paper = settings.paper;
    paper.max_trade_value_paise = 1_000_000;
    let policy = RoundTripPolicy {
        symbol: Mutex::new(None),
    };
    let report = run_agent_backtest(&bars, &policy, &paper, &SyntheticConfig::default(), 2)
        .await
        .unwrap();
    assert_eq!(report.decisions, 2);
    assert_eq!(report.trades.len(), 1);
    assert!(report.ending_equity_rupees.is_finite());
}

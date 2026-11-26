use crate::types::{MarketSnapshot, Quote, Side, Signal, TradeOutcome};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc, Weekday};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Clone, Debug)]
pub struct IndexBar {
    pub timestamp: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OptionRight {
    #[serde(rename = "CE")]
    Call,
    #[serde(rename = "PE")]
    Put,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyntheticOptionBar {
    pub symbol: String,
    pub expiry: NaiveDate,
    pub strike: u32,
    pub right: OptionRight,
    pub lot_size: u32,
    pub iv: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub bid: f64,
    pub ask: f64,
}

#[derive(Clone, Debug)]
pub struct SyntheticConfig {
    pub strike_step: u32,
    pub strikes_each_side: u32,
    pub lot_size: u32,
    pub fallback_iv: f64,
    pub risk_free_rate: f64,
    pub spread_bps: u32,
    pub bars_per_year: f64,
    pub iv_lookback_bars: usize,
}

#[derive(Clone, Debug)]
pub struct BacktestPosition {
    pub symbol: String,
    pub expiry: NaiveDate,
    pub strike: u32,
    pub right: OptionRight,
    pub lots: u32,
    pub lot_size: u32,
    pub entry_price: f64,
    pub target_price: f64,
    pub stop_loss_price: f64,
    pub trailing_stop_loss_price: f64,
    pub trailing_distance: f64,
    pub highest_option_price: f64,
    pub entry_fees: f64,
    pub entry_time: DateTime<Utc>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BacktestTrade {
    pub symbol: String,
    pub lots: u32,
    pub entry_time: DateTime<Utc>,
    pub exit_time: DateTime<Utc>,
    pub entry_price: f64,
    pub target_price: f64,
    pub stop_loss_price: f64,
    pub trailing_stop_loss_price: f64,
    pub exit_price: f64,
    pub fees: f64,
    pub pnl_rupees: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BacktestReport {
    pub bars_seen: usize,
    pub decisions: usize,
    pub starting_capital_rupees: f64,
    pub ending_equity_rupees: f64,
    pub net_pnl_rupees: f64,
    pub max_drawdown_rupees: f64,
    pub trades: Vec<BacktestTrade>,
}

#[async_trait]
pub trait BacktestPolicy: Send + Sync {
    async fn decide(
        &self,
        snapshot: &MarketSnapshot,
        position: Option<&BacktestPosition>,
    ) -> Result<Option<Signal>>;

    async fn on_trade_closed(&self, _outcome: &TradeOutcome) -> Result<()> {
        Ok(())
    }
}

impl Default for SyntheticConfig {
    fn default() -> Self {
        Self {
            strike_step: 50,
            strikes_each_side: 5,
            lot_size: 65,
            fallback_iv: 0.20,
            risk_free_rate: 0.065,
            spread_bps: 20,
            bars_per_year: 252.0 * 75.0,
            iv_lookback_bars: 20 * 75,
        }
    }
}

pub fn config_for_bars(bars: &[IndexBar]) -> SyntheticConfig {
    let mut config = SyntheticConfig::default();
    if bars
        .get(1)
        .zip(bars.first())
        .is_some_and(|(next, first)| next.timestamp - first.timestamp >= chrono::Duration::hours(6))
    {
        config.bars_per_year = 252.0;
        config.iv_lookback_bars = 20;
    }
    config
}

/// Parse Yahoo Finance's chart JSON into time-ordered NIFTY index bars.
pub fn load_yahoo_chart(path: impl AsRef<Path>) -> Result<Vec<IndexBar>> {
    let data: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    let result = data["chart"]["result"][0]
        .as_object()
        .context("Yahoo chart response has no result")?;
    let times = result["timestamp"]
        .as_array()
        .context("Yahoo chart has no timestamps")?;
    let quote = &result["indicators"]["quote"][0];
    let field = |name: &str| {
        quote[name]
            .as_array()
            .with_context(|| format!("Yahoo chart missing {name}"))
    };
    let opens = field("open")?;
    let highs = field("high")?;
    let lows = field("low")?;
    let closes = field("close")?;
    let volumes = field("volume")?;
    let mut bars = Vec::new();
    for i in 0..times.len() {
        let number = |v: &serde_json::Value| v.as_f64().filter(|x| x.is_finite());
        let (Some(ts), Some(open), Some(high), Some(low), Some(close)) = (
            times[i].as_i64(),
            number(&opens[i]),
            number(&highs[i]),
            number(&lows[i]),
            number(&closes[i]),
        ) else {
            continue;
        };
        if low <= 0.0 || high < low || open < low || open > high || close < low || close > high {
            continue;
        }
        bars.push(IndexBar {
            timestamp: Utc
                .timestamp_opt(ts, 0)
                .single()
                .context("invalid Yahoo timestamp")?,
            open,
            high,
            low,
            close,
            volume: volumes.get(i).and_then(|v| v.as_u64()).unwrap_or(0),
        });
    }
    if bars.len() < 2 {
        bail!("need at least two valid Yahoo NIFTY bars")
    }
    bars.sort_by_key(|b| b.timestamp);
    bars.dedup_by_key(|b| b.timestamp);
    Ok(bars)
}

/// Read OHLC JSON returned by the connected INDmoney tools. Their `datetime_ist` field is wall-clock IST.
pub fn load_indmoney_ohlc(path: impl AsRef<Path>) -> Result<Vec<IndexBar>> {
    let data: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    let candles = data["candles"]
        .as_array()
        .context("INDmoney data has no candles array")?;
    let mut bars = Vec::new();
    for candle in candles {
        let Some(dt) = candle["datetime_ist"].as_str() else {
            continue;
        };
        let naive = chrono::NaiveDateTime::parse_from_str(dt, "%Y-%m-%d %H:%M:%S")
            .context("invalid INDmoney datetime_ist")?;
        let (Some(open), Some(high), Some(low), Some(close)) = (
            candle["open"].as_f64(),
            candle["high"].as_f64(),
            candle["low"].as_f64(),
            candle["close"].as_f64(),
        ) else {
            continue;
        };
        if ![open, high, low, close].iter().all(|v| v.is_finite())
            || low <= 0.0
            || high < low
            || open < low
            || open > high
            || close < low
            || close > high
        {
            continue;
        }
        // The INDmoney daily OHLC endpoint labels daily bars at 05:30 IST; treat those as session-close bars.
        let local = if naive.time() == chrono::NaiveTime::from_hms_opt(5, 30, 0).unwrap() {
            naive.date().and_hms_opt(15, 30, 0).unwrap()
        } else {
            naive
        };
        let timestamp = Utc.from_utc_datetime(&(local - chrono::Duration::minutes(330)));
        bars.push(IndexBar {
            timestamp,
            open,
            high,
            low,
            close,
            volume: candle["volume"].as_u64().unwrap_or(0),
        });
    }
    bars.sort_by_key(|b| b.timestamp);
    bars.dedup_by_key(|b| b.timestamp);
    if bars.len() < 2 {
        bail!("need at least two valid INDmoney candles")
    }
    Ok(bars)
}

pub fn load_index_history(path: impl AsRef<Path>) -> Result<Vec<IndexBar>> {
    let data: serde_json::Value = serde_json::from_slice(&fs::read(path.as_ref())?)?;
    if data.get("chart").is_some() {
        load_yahoo_chart(path)
    } else {
        load_indmoney_ohlc(path)
    }
}

pub async fn fetch_yahoo_nifty_5m(path: impl AsRef<Path>) -> Result<Vec<IndexBar>> {
    let response=reqwest::Client::new().get("https://query1.finance.yahoo.com/v8/finance/chart/%5ENSEI?range=60d&interval=5m&includePrePost=false")
        .header(reqwest::header::USER_AGENT,"nifty-paper-agent/0.1 personal-backtest")
        .send().await?.error_for_status()?.bytes().await?;
    if let Some(parent) = path.as_ref().parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path.as_ref(), &response)?;
    load_yahoo_chart(path)
}

/// NIFTY weekly expiry weekday changed from Thursday to Tuesday for expiries from Sep 2025.
/// This calendar does not adjust for exchange holidays; holiday dates should be supplied in a future calendar import.
pub fn weekly_expiry(date: NaiveDate) -> NaiveDate {
    let cutover = NaiveDate::from_ymd_opt(2025, 9, 1).unwrap();
    let target = if date >= cutover {
        Weekday::Tue
    } else {
        Weekday::Thu
    };
    let delta = (target.num_days_from_monday() as i64
        - date.weekday().num_days_from_monday() as i64)
        .rem_euclid(7);
    date + chrono::Duration::days(delta)
}

pub fn rolling_volatility(closes: &[f64], bars_per_year: f64, fallback: f64) -> f64 {
    if closes.len() < 3 || bars_per_year <= 0.0 {
        return fallback;
    }
    let returns: Vec<f64> = closes
        .windows(2)
        .filter_map(|w| (w[0] > 0.0 && w[1] > 0.0).then(|| (w[1] / w[0]).ln()))
        .collect();
    if returns.len() < 2 {
        return fallback;
    }
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let variance =
        returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (returns.len() - 1) as f64;
    (variance.sqrt() * bars_per_year.sqrt()).clamp(0.08, 1.5)
}

pub fn synthetic_chain(
    bar: &IndexBar,
    iv: f64,
    config: &SyntheticConfig,
) -> Result<Vec<SyntheticOptionBar>> {
    if !bar.close.is_finite() || bar.close <= 0.0 || config.strike_step == 0 || config.lot_size == 0
    {
        bail!("invalid spot bar or synthetic-chain settings")
    }
    let iv = if iv.is_finite() {
        iv.clamp(0.08, 1.5)
    } else {
        config.fallback_iv
    };
    let date = bar.timestamp.date_naive();
    let expiry = weekly_expiry(date);
    let atm = ((bar.close / config.strike_step as f64).round() as u32) * config.strike_step;
    let mut chain = Vec::new();
    for offset in -(config.strikes_each_side as i32)..=(config.strikes_each_side as i32) {
        let strike = (atm as i64 + offset as i64 * config.strike_step as i64)
            .max(config.strike_step as i64) as u32;
        for right in [OptionRight::Call, OptionRight::Put] {
            chain.push(synthetic_contract(bar, expiry, strike, right, iv, config)?);
        }
    }
    Ok(chain)
}

fn synthetic_contract(
    bar: &IndexBar,
    expiry: NaiveDate,
    strike: u32,
    right: OptionRight,
    iv: f64,
    config: &SyntheticConfig,
) -> Result<SyntheticOptionBar> {
    let expiry_close = Utc
        .with_ymd_and_hms(expiry.year(), expiry.month(), expiry.day(), 10, 0, 0)
        .single()
        .context("invalid expiry timestamp")?;
    let years =
        ((expiry_close - bar.timestamp).num_seconds().max(60) as f64) / (365.25 * 24.0 * 3600.0);
    let price =
        |spot: f64| black_scholes(spot, strike as f64, years, config.risk_free_rate, iv, right);
    let open = price(bar.open);
    let high = if right == OptionRight::Call {
        price(bar.high)
    } else {
        price(bar.low)
    };
    let low = if right == OptionRight::Call {
        price(bar.low)
    } else {
        price(bar.high)
    };
    let close = price(bar.close);
    let half = close * config.spread_bps as f64 / 20_000.0;
    let symbol = format!(
        "NIFTY-{expiry}-{strike}-{}",
        if right == OptionRight::Call {
            "CE"
        } else {
            "PE"
        }
    );
    Ok(SyntheticOptionBar {
        symbol,
        expiry,
        strike,
        right,
        lot_size: config.lot_size,
        iv,
        open,
        high: high.max(low),
        low: high.min(low),
        close,
        bid: (close - half).max(0.05),
        ask: close + half,
    })
}

pub async fn run_agent_backtest<P: BacktestPolicy>(
    bars: &[IndexBar],
    policy: &P,
    paper: &crate::config::Paper,
    config: &SyntheticConfig,
    max_decisions: usize,
) -> Result<BacktestReport> {
    if bars.len() < 2 {
        bail!("backtest needs at least two index bars")
    }
    if max_decisions == 0 {
        bail!("max_decisions must be greater than zero")
    }
    if bars.windows(2).any(|w| w[0].timestamp >= w[1].timestamp) {
        bail!("index bars must be strictly time ordered")
    }
    let starting = paper.starting_capital_paise as f64 / 100.0;
    let max_trade = paper.max_trade_value_paise as f64 / 100.0;
    let mut cash = starting;
    let mut position: Option<BacktestPosition> = None;
    let mut trades = Vec::new();
    let mut decisions = 0;
    let mut max_equity = starting;
    let mut max_dd: f64 = 0.0;
    let mut daily_pnl = std::collections::HashMap::<NaiveDate, f64>::new();
    let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
    let last_index = bars.len() - 1;
    let end_bar_index = last_index.min(max_decisions);
    for i in 0..end_bar_index {
        let lookback_start = (i + 1).saturating_sub(config.iv_lookback_bars.max(2));
        let iv = rolling_volatility(
            &closes[lookback_start..=i],
            config.bars_per_year,
            config.fallback_iv,
        );
        let mut chain = synthetic_chain(&bars[i], iv, config)?;
        if let Some(p) = &position {
            if !chain.iter().any(|q| q.symbol == p.symbol) {
                chain.push(synthetic_contract(
                    &bars[i], p.expiry, p.strike, p.right, iv, config,
                )?);
            }
        }
        // Agent-selected target and protective levels are monitored against each completed
        // synthetic option candle. If a candle touches both stop and target, assume the stop
        // was hit first because OHLC bars do not reveal intrabar ordering.
        if let Some(p) = position.as_mut() {
            if let Some(q) = chain.iter().find(|q| q.symbol == p.symbol) {
                let protective_stop = p.stop_loss_price.max(p.trailing_stop_loss_price);
                let exit_raw = if q.low <= protective_stop {
                    Some(if q.open <= protective_stop {
                        q.open
                    } else {
                        protective_stop
                    })
                } else if q.high >= p.target_price {
                    Some(if q.open >= p.target_price {
                        q.open
                    } else {
                        p.target_price
                    })
                } else {
                    None
                };
                if let Some(exit_raw) = exit_raw {
                    let p = position.take().unwrap();
                    let fill = (exit_raw
                        * (1.0
                            - config.spread_bps as f64 / 20_000.0
                            - paper.slippage_bps as f64 / 10_000.0))
                        .max(0.05);
                    let fees =
                        fill * p.lot_size as f64 * p.lots as f64 * paper.fee_bps as f64 / 10_000.0;
                    cash += fill * p.lot_size as f64 * p.lots as f64 - fees;
                    let pnl = (fill - p.entry_price) * p.lot_size as f64 * p.lots as f64
                        - p.entry_fees
                        - fees;
                    let exit_reason = if q.low <= protective_stop {
                        "protective_stop"
                    } else {
                        "target_hit"
                    };
                    let trade = BacktestTrade {
                        symbol: p.symbol,
                        lots: p.lots,
                        entry_time: p.entry_time,
                        exit_time: bars[i].timestamp,
                        entry_price: p.entry_price,
                        target_price: p.target_price,
                        stop_loss_price: p.stop_loss_price,
                        trailing_stop_loss_price: p.trailing_stop_loss_price,
                        exit_price: fill,
                        fees: p.entry_fees + fees,
                        pnl_rupees: pnl,
                    };
                    let return_bps = if trade.entry_price > 0.0 {
                        (((fill - trade.entry_price) / trade.entry_price) * 10_000.0).round()
                            as i32
                    } else {
                        0
                    };
                    let outcome = TradeOutcome {
                        trade_id: format!("bt-{}-{}", trade.symbol, trade.exit_time.timestamp()),
                        symbol: trade.symbol.clone(),
                        side: Side::Buy,
                        quantity: trade.lots,
                        entry_price: trade.entry_price,
                        exit_price: trade.exit_price,
                        pnl_rupees: trade.pnl_rupees,
                        return_bps,
                        exit_reason: exit_reason.into(),
                        entry_time: trade.entry_time,
                        exit_time: trade.exit_time,
                        situation_summary: format!("Backtest bar spot close={:.2}", bars[i].close),
                        regime_tags: vec!["nifty".into(), "backtest".into()],
                        decision_rationale: exit_reason.into(),
                    };
                    trades.push(trade);
                    *daily_pnl.entry(bars[i].timestamp.date_naive()).or_default() += pnl;
                    policy.on_trade_closed(&outcome).await?;
                } else {
                    p.highest_option_price = p.highest_option_price.max(q.high);
                    p.trailing_stop_loss_price = p
                        .trailing_stop_loss_price
                        .max(p.highest_option_price - p.trailing_distance);
                }
            }
        }
        let snapshot=MarketSnapshot {id:uuid::Uuid::new_v4(),created_at:bars[i].timestamp,
            quotes:chain.iter().map(|q|Quote{symbol:q.symbol.clone(),bid:q.bid,ask:q.ask,last:q.close,timestamp:bars[i].timestamp}).collect(),
            context:format!("HISTORICAL NIFTY BAR; spot O/H/L/C={:.2}/{:.2}/{:.2}/{:.2}; volume={}; synthetic_iv={:.4}; expiry schedule uses current NSE Tuesday rule after 2025-09 and prior Thursday rule before. {}",
                bars[i].open,bars[i].high,bars[i].low,bars[i].close,bars[i].volume,iv,position.as_ref().map(|p|format!("OPEN LONG {} lots={} agent_entry={:.2} agent_target={:.2} agent_stop={:.2} current_agent_trail={:.2}",p.symbol,p.lots,p.entry_price,p.target_price,p.stop_loss_price,p.trailing_stop_loss_price)).unwrap_or_else(||"FLAT".into()))};
        let marked = position
            .as_ref()
            .map(|p| {
                chain
                    .iter()
                    .find(|q| q.symbol == p.symbol)
                    .map(|q| q.bid * p.lot_size as f64 * p.lots as f64)
                    .unwrap_or(0.0)
            })
            .unwrap_or(0.0);
        let equity = cash + marked;
        max_equity = max_equity.max(equity);
        max_dd = max_dd.max(max_equity - equity);
        decisions += 1;
        let Some(signal) = policy.decide(&snapshot, position.as_ref()).await? else {
            continue;
        };
        if crate::risk::check_agent_plan(&signal).is_err() {
            continue;
        }
        if signal.snapshot_id.is_some_and(|id| id != snapshot.id) || signal.confidence_bps > 10_000
        {
            continue;
        }
        let Some(contract) = chain.iter().find(|q| q.symbol == signal.symbol).cloned() else {
            continue;
        };
        let next = &bars[i + 1];
        let next_iv = iv; // Avoid look-ahead: use only volatility known at the decision bar.
        let next_contract = synthetic_contract(
            next,
            contract.expiry,
            contract.strike,
            contract.right,
            next_iv,
            config,
        )?;
        if signal.side == Side::Buy && position.is_none() {
            let lots = signal.quantity.max(1).min(1); // one-lot long options only in this starter
            if daily_pnl
                .get(&next.timestamp.date_naive())
                .copied()
                .unwrap_or(0.0)
                <= -(paper.max_daily_loss_paise as f64 / 100.0)
            {
                continue;
            }
            let fill = next_contract.open
                * (1.0
                    + config.spread_bps as f64 / 20_000.0
                    + paper.slippage_bps as f64 / 10_000.0);
            if fill > signal.entry_price
                || fill <= signal.stop_loss_price
                || fill <= signal.trailing_stop_loss_price
                || fill >= signal.target_price
                || fill * contract.lot_size as f64 * lots as f64 > max_trade
                || fill * contract.lot_size as f64 * lots as f64 > cash
            {
                continue;
            }
            let fees =
                fill * contract.lot_size as f64 * lots as f64 * paper.fee_bps as f64 / 10_000.0;
            let total = fill * contract.lot_size as f64 * lots as f64 + fees;
            if total > cash {
                continue;
            }
            cash -= total;
            position = Some(BacktestPosition {
                symbol: contract.symbol,
                expiry: contract.expiry,
                strike: contract.strike,
                right: contract.right,
                lots,
                lot_size: contract.lot_size,
                entry_price: fill,
                target_price: signal.target_price,
                stop_loss_price: signal.stop_loss_price,
                trailing_stop_loss_price: signal.trailing_stop_loss_price,
                trailing_distance: signal.entry_price - signal.trailing_stop_loss_price,
                highest_option_price: fill,
                entry_fees: fees,
                entry_time: next.timestamp,
            });
        } else if signal.side == Side::Sell {
            if let Some(p) = position.take() {
                if p.symbol != signal.symbol {
                    position = Some(p);
                    continue;
                }
                let fill = (next_contract.open
                    * (1.0
                        - config.spread_bps as f64 / 20_000.0
                        - paper.slippage_bps as f64 / 10_000.0))
                    .max(0.05);
                let fees =
                    fill * p.lot_size as f64 * p.lots as f64 * paper.fee_bps as f64 / 10_000.0;
                cash += fill * p.lot_size as f64 * p.lots as f64 - fees;
                let pnl = (fill - p.entry_price) * p.lot_size as f64 * p.lots as f64
                    - p.entry_fees
                    - fees;
                let trade = BacktestTrade {
                    symbol: p.symbol,
                    lots: p.lots,
                    entry_time: p.entry_time,
                    exit_time: next.timestamp,
                    entry_price: p.entry_price,
                    target_price: p.target_price,
                    stop_loss_price: p.stop_loss_price,
                    trailing_stop_loss_price: p.trailing_stop_loss_price,
                    exit_price: fill,
                    fees: p.entry_fees + fees,
                    pnl_rupees: pnl,
                };
                let return_bps = if trade.entry_price > 0.0 {
                    (((fill - trade.entry_price) / trade.entry_price) * 10_000.0).round() as i32
                } else {
                    0
                };
                let outcome = TradeOutcome {
                    trade_id: signal.id.clone(),
                    symbol: trade.symbol.clone(),
                    side: Side::Buy,
                    quantity: trade.lots,
                    entry_price: trade.entry_price,
                    exit_price: trade.exit_price,
                    pnl_rupees: trade.pnl_rupees,
                    return_bps,
                    exit_reason: "agent_exit".into(),
                    entry_time: trade.entry_time,
                    exit_time: trade.exit_time,
                    situation_summary: snapshot.context.clone(),
                    regime_tags: vec!["nifty".into(), "backtest".into()],
                    decision_rationale: signal.rationale.clone(),
                };
                trades.push(trade);
                *daily_pnl.entry(next.timestamp.date_naive()).or_default() += pnl;
                policy.on_trade_closed(&outcome).await?;
            }
        }
    }
    if let Some(p) = position {
        let bar = &bars[end_bar_index];
        let start = (end_bar_index + 1).saturating_sub(config.iv_lookback_bars.max(2));
        let iv = rolling_volatility(
            &closes[start..=end_bar_index],
            config.bars_per_year,
            config.fallback_iv,
        );
        let q = synthetic_contract(bar, p.expiry, p.strike, p.right, iv, config)?;
        let exit = (q.close
            * (1.0 - config.spread_bps as f64 / 20_000.0 - paper.slippage_bps as f64 / 10_000.0))
            .max(0.05);
        let fees = exit * p.lot_size as f64 * p.lots as f64 * paper.fee_bps as f64 / 10_000.0;
        cash += exit * p.lot_size as f64 * p.lots as f64 - fees;
        let pnl =
            (exit - p.entry_price) * p.lot_size as f64 * p.lots as f64 - p.entry_fees - fees;
        let trade = BacktestTrade {
            symbol: p.symbol,
            lots: p.lots,
            entry_time: p.entry_time,
            exit_time: bar.timestamp,
            entry_price: p.entry_price,
            target_price: p.target_price,
            stop_loss_price: p.stop_loss_price,
            trailing_stop_loss_price: p.trailing_stop_loss_price,
            exit_price: exit,
            fees: p.entry_fees + fees,
            pnl_rupees: pnl,
        };
        let return_bps = if trade.entry_price > 0.0 {
            (((exit - trade.entry_price) / trade.entry_price) * 10_000.0).round() as i32
        } else {
            0
        };
        let outcome = TradeOutcome {
            trade_id: format!("bt-eod-{}-{}", trade.symbol, trade.exit_time.timestamp()),
            symbol: trade.symbol.clone(),
            side: Side::Buy,
            quantity: trade.lots,
            entry_price: trade.entry_price,
            exit_price: trade.exit_price,
            pnl_rupees: trade.pnl_rupees,
            return_bps,
            exit_reason: "end_of_backtest".into(),
            entry_time: trade.entry_time,
            exit_time: trade.exit_time,
            situation_summary: format!("End of backtest mark-to-market spot={:.2}", bar.close),
            regime_tags: vec!["nifty".into(), "backtest".into()],
            decision_rationale: "end_of_backtest".into(),
        };
        trades.push(trade);
        policy.on_trade_closed(&outcome).await?;
    }
    Ok(BacktestReport {
        bars_seen: end_bar_index + 1,
        decisions,
        starting_capital_rupees: starting,
        ending_equity_rupees: cash,
        net_pnl_rupees: cash - starting,
        max_drawdown_rupees: max_dd,
        trades,
    })
}

fn black_scholes(
    spot: f64,
    strike: f64,
    years: f64,
    rate: f64,
    iv: f64,
    right: OptionRight,
) -> f64 {
    let intrinsic = match right {
        OptionRight::Call => (spot - strike).max(0.0),
        OptionRight::Put => (strike - spot).max(0.0),
    };
    if years <= 1.0 / (365.25 * 24.0 * 60.0) || iv <= 0.0 {
        return intrinsic;
    }
    let vol_time = iv * years.sqrt();
    let d1 = ((spot / strike).ln() + (rate + 0.5 * iv * iv) * years) / vol_time;
    let d2 = d1 - vol_time;
    let call = spot * normal_cdf(d1) - strike * (-rate * years).exp() * normal_cdf(d2);
    match right {
        OptionRight::Call => call.max(intrinsic),
        OptionRight::Put => (call - spot + strike * (-rate * years).exp()).max(intrinsic),
    }
}

fn normal_cdf(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.2316419 * z);
    let d = 0.3989422804014327 * (-0.5 * z * z).exp();
    let p = 1.0
        - d * t
            * (0.319381530
                + t * (-0.356563782 + t * (1.781477937 + t * (-1.821255978 + t * 1.330274429))));
    if x >= 0.0 {
        p
    } else {
        1.0 - p
    }
}

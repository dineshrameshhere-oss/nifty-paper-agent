use crate::{
    config::Paper,
    types::{MarketSnapshot, Quote, Side, Signal},
};
use anyhow::{bail, Result};

pub fn check_signal(
    signal: &Signal,
    quote: &Quote,
    paper: &Paper,
    cash_paise: i64,
    open_positions: usize,
    daily_loss_paise: i64,
) -> Result<()> {
    if signal.id.trim().is_empty() || signal.symbol != quote.symbol {
        bail!("signal identity/symbol invalid")
    }
    if signal.quantity == 0 || signal.quantity > paper.max_position_quantity {
        bail!("quantity limit")
    }
    if !signal.limit_price.is_finite() || signal.limit_price <= 0.0 {
        bail!("invalid price")
    }
    if signal.confidence_bps > 10_000 {
        bail!("confidence out of range")
    }
    if daily_loss_paise >= paper.max_daily_loss_paise {
        bail!("daily loss limit")
    }
    if open_positions >= paper.max_open_positions {
        bail!("open position limit")
    }
    let value = signal.limit_price * signal.quantity as f64 * 100.0;
    if value > paper.max_trade_value_paise as f64 {
        bail!("per-trade capital limit")
    }
    if signal.side == Side::Buy && value > cash_paise as f64 {
        bail!("insufficient paper capital")
    }
    if signal.side == Side::Sell && !paper.allow_short {
        bail!("short selling disabled")
    }
    Ok(())
}

pub fn check_market_snapshot(snapshot: &MarketSnapshot, stale_after_seconds: i64) -> Result<()> {
    let now = chrono::Utc::now();
    if snapshot.quotes.is_empty() {
        bail!("empty snapshot")
    }
    if now - snapshot.created_at > chrono::Duration::seconds(stale_after_seconds) {
        bail!("stale snapshot")
    }
    for quote in &snapshot.quotes {
        crate::data::ensure_fresh(quote, stale_after_seconds)?;
    }
    Ok(())
}

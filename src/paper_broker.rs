use crate::{
    config::Paper,
    risk,
    types::{Fill, MarketSnapshot, Quote, Side, Signal},
};
use anyhow::{bail, Result};
use chrono::Utc;

pub struct PaperBroker {
    pub cash_paise: i64,
    pub config: Paper,
}
impl PaperBroker {
    pub fn new(config: Paper) -> Self {
        Self {
            cash_paise: config.starting_capital_paise,
            config,
        }
    }
    pub fn execute(
        &mut self,
        signal: &Signal,
        quote: &Quote,
        snapshot: &MarketSnapshot,
        open_positions: usize,
        daily_loss_paise: i64,
        stale_after_seconds: i64,
    ) -> Result<Fill> {
        risk::check_market_snapshot(snapshot, stale_after_seconds)?;
        if signal.snapshot_id.is_some_and(|id| id != snapshot.id) {
            bail!("signal snapshot mismatch")
        }
        risk::check_signal(
            signal,
            quote,
            &self.config,
            self.cash_paise,
            open_positions,
            daily_loss_paise,
        )?;
        let quantity = ((signal.quantity as f64 * self.config.partial_fill_fraction).floor()
            as u32)
            .min(signal.quantity);
        if quantity == 0
            || !self.config.partial_fill_fraction.is_finite()
            || self.config.partial_fill_fraction <= 0.0
        {
            bail!("simulated zero fill")
        }
        let mid = (quote.bid + quote.ask) / 2.0;
        let direction = if signal.side == Side::Buy { 1.0 } else { -1.0 };
        let px = mid
            * (1.0
                + direction
                    * (self.config.spread_bps as f64 / 2.0 + self.config.slippage_bps as f64)
                    / 10_000.0);
        if signal.side == Side::Buy
            && (px <= signal.stop_loss_price
                || px <= signal.trailing_stop_loss_price
                || px >= signal.target_price)
        {
            bail!("simulated entry falls outside the agent-selected trade plan")
        }
        if (signal.side == Side::Buy && px > signal.entry_price)
            || (signal.side == Side::Sell && px < signal.entry_price)
        {
            bail!("simulated price exceeds signal limit")
        }
        let fees = px * quantity as f64 * self.config.fee_bps as f64 / 10_000.0;
        let gross = (px * quantity as f64
            + if signal.side == Side::Buy {
                fees
            } else {
                -fees
            })
            * 100.0;
        if signal.side == Side::Buy && gross > self.cash_paise as f64 {
            bail!("insufficient cash after simulated costs")
        }
        let fill = Fill {
            signal_id: signal.id.clone(),
            symbol: signal.symbol.clone(),
            side: signal.side.clone(),
            quantity,
            price: px,
            fees,
            timestamp: Utc::now(),
        };
        if signal.side == Side::Buy {
            self.cash_paise -= gross.round() as i64;
        } else {
            self.cash_paise += gross.round() as i64;
        }
        Ok(fill)
    }
}

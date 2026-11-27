use crate::{
    config::Paper,
    risk,
    types::{Fill, MarketSnapshot, Quote, Side, Signal},
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct PaperBroker {
    pub cash_paise: i64,
    pub config: Paper,
    positions: BTreeMap<String, u32>,
    avg_entry_prices: BTreeMap<String, f64>,
    open_entry_fees: BTreeMap<String, f64>,
    realized_pnl_paise: i64,
}
impl PaperBroker {
    pub fn new(config: Paper) -> Self {
        Self {
            cash_paise: config.starting_capital_paise,
            config,
            positions: BTreeMap::new(),
            avg_entry_prices: BTreeMap::new(),
            open_entry_fees: BTreeMap::new(),
            realized_pnl_paise: 0,
        }
    }

    pub fn open_position_qty(&self, symbol: &str) -> u32 {
        self.positions.get(symbol).copied().unwrap_or(0)
    }

    pub fn open_positions_count(&self) -> usize {
        self.positions.len()
    }

    pub fn open_symbols(&self) -> Vec<String> {
        self.positions.keys().cloned().collect()
    }

    pub fn open_positions(&self) -> Vec<(String, u32)> {
        self.positions.iter().map(|(k, &v)| (k.clone(), v)).collect()
    }

    pub fn avg_entry_price(&self, symbol: &str) -> Option<f64> {
        self.avg_entry_prices.get(symbol).copied()
    }

    pub fn realized_pnl_paise(&self) -> i64 {
        self.realized_pnl_paise
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute(
        &mut self,
        signal: &Signal,
        quote: &Quote,
        snapshot: &MarketSnapshot,
        open_positions: usize,
        daily_loss_paise: i64,
        stale_after_seconds: i64,
    ) -> Result<Fill> {
        self.execute_at(
            signal,
            quote,
            snapshot,
            open_positions,
            daily_loss_paise,
            stale_after_seconds,
            Utc::now(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_at(
        &mut self,
        signal: &Signal,
        quote: &Quote,
        snapshot: &MarketSnapshot,
        open_positions: usize,
        daily_loss_paise: i64,
        stale_after_seconds: i64,
        now: DateTime<Utc>,
    ) -> Result<Fill> {
        risk::check_market_snapshot_at(snapshot, stale_after_seconds, now)?;
        crate::data::ensure_fresh_at(quote, stale_after_seconds, now)?;
        if signal.snapshot_id.is_some_and(|id| id != snapshot.id) {
            bail!("signal snapshot mismatch")
        }
        let open_symbol_qty = self.open_position_qty(&signal.symbol);
        let effective_open_positions = open_positions.max(self.open_positions_count());
        risk::check_signal_with_open_qty(
            signal,
            quote,
            &self.config,
            self.cash_paise,
            effective_open_positions,
            daily_loss_paise,
            open_symbol_qty,
        )?;
        if !self.config.partial_fill_fraction.is_finite()
            || self.config.partial_fill_fraction <= 0.0
        {
            bail!("simulated zero fill")
        }
        let quantity = ((signal.quantity as f64 * self.config.partial_fill_fraction).floor()
            as u32)
            .min(signal.quantity);
        if quantity == 0 {
            bail!("simulated zero fill")
        }
        let mid = (quote.bid + quote.ask) / 2.0;
        let direction = if signal.side == Side::Buy { 1.0 } else { -1.0 };
        let px = mid
            * (1.0
                + direction
                    * (self.config.spread_bps as f64 / 2.0 + self.config.slippage_bps as f64)
                    / 10_000.0);
        if !px.is_finite() || px <= 0.0 {
            bail!("invalid simulated execution price")
        }
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
        if signal.side == Side::Sell && quantity > open_symbol_qty {
            bail!("short positions are not supported by PaperBroker long-only tracker")
        }
        let fees = px * quantity as f64 * self.config.fee_bps as f64 / 10_000.0;
        let gross = (px * quantity as f64
            + if signal.side == Side::Buy {
                fees
            } else {
                -fees
            })
            * 100.0;
        if !fees.is_finite() || !gross.is_finite() {
            bail!("invalid simulated execution costs")
        }
        if signal.side == Side::Buy && risk::exceeds_i64(gross, self.cash_paise) {
            bail!("insufficient cash after simulated costs")
        }
        let fill = Fill {
            signal_id: signal.id.clone(),
            symbol: signal.symbol.clone(),
            side: signal.side,
            quantity,
            price: px,
            fees,
            timestamp: now,
        };
        if signal.side == Side::Buy {
            let gross_paise = checked_rounded_i64(gross, "simulated execution cash overflow")?;
            let new_cash_paise = self
                .cash_paise
                .checked_sub(gross_paise)
                .context("cash paise overflow")?;
            let prev_qty = open_symbol_qty;
            let prev_avg = self
                .avg_entry_prices
                .get(&signal.symbol)
                .copied()
                .unwrap_or(0.0);
            let new_qty = prev_qty
                .checked_add(quantity)
                .context("position quantity overflow")?;
            let new_avg = if prev_qty == 0 {
                px
            } else {
                (prev_qty as f64 * prev_avg + quantity as f64 * px) / new_qty as f64
            };
            if !new_avg.is_finite() {
                bail!("average entry price overflow")
            }
            let prev_entry_fees = self
                .open_entry_fees
                .get(&signal.symbol)
                .copied()
                .unwrap_or(0.0);
            let new_entry_fees = prev_entry_fees + fees;
            if !new_entry_fees.is_finite() {
                bail!("entry fees overflow")
            }
            self.cash_paise = new_cash_paise;
            self.positions.insert(signal.symbol.clone(), new_qty);
            self.avg_entry_prices.insert(signal.symbol.clone(), new_avg);
            self.open_entry_fees
                .insert(signal.symbol.clone(), new_entry_fees);
        } else {
            let gross_paise = checked_rounded_i64(gross, "simulated execution cash overflow")?;
            let new_cash_paise = self
                .cash_paise
                .checked_add(gross_paise)
                .context("cash paise overflow")?;
            let prev_qty = open_symbol_qty;
            let prev_avg = self
                .avg_entry_prices
                .get(&signal.symbol)
                .copied()
                .unwrap_or(0.0);
            let total_entry_fees = self
                .open_entry_fees
                .get(&signal.symbol)
                .copied()
                .unwrap_or(0.0);
            let alloc_entry_fees = if quantity == prev_qty {
                total_entry_fees
            } else {
                total_entry_fees * (quantity as f64 / prev_qty as f64)
            };
            let pnl_rupees = (px - prev_avg) * quantity as f64 - alloc_entry_fees - fees;
            let pnl_paise = checked_rounded_i64(pnl_rupees * 100.0, "realized pnl overflow")?;
            let new_realized_pnl_paise = self
                .realized_pnl_paise
                .checked_add(pnl_paise)
                .context("realized pnl overflow")?;
            let rem_qty = prev_qty
                .checked_sub(quantity)
                .context("position quantity underflow")?;
            self.cash_paise = new_cash_paise;
            self.realized_pnl_paise = new_realized_pnl_paise;
            if rem_qty == 0 {
                self.positions.remove(&signal.symbol);
                self.avg_entry_prices.remove(&signal.symbol);
                self.open_entry_fees.remove(&signal.symbol);
            } else {
                self.positions.insert(signal.symbol.clone(), rem_qty);
                self.open_entry_fees
                    .insert(signal.symbol.clone(), total_entry_fees - alloc_entry_fees);
            }
        }
        Ok(fill)
    }
}

fn checked_rounded_i64(val: f64, context: &str) -> Result<i64> {
    let rounded = val.round();
    if !rounded.is_finite() || rounded < i64::MIN as f64 || rounded >= -(i64::MIN as f64) {
        bail!("{context}")
    }
    Ok(rounded as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use uuid::Uuid;

    fn sample_paper() -> Paper {
        Paper {
            starting_capital_paise: 1_000_000,
            max_trade_value_paise: 200_000,
            max_open_positions: 3,
            max_daily_loss_paise: 50_000,
            max_position_quantity: 50,
            allow_short: false,
            spread_bps: 10,
            slippage_bps: 5,
            fee_bps: 3,
            partial_fill_fraction: 1.0,
        }
    }

    fn make_snapshot(symbol: &str, bid: f64, ask: f64) -> (MarketSnapshot, Quote) {
        let now = Utc::now();
        let quote = Quote {
            symbol: symbol.into(),
            bid,
            ask,
            last: (bid + ask) / 2.0,
            timestamp: now,
        };
        let snapshot = MarketSnapshot {
            id: Uuid::new_v4(),
            created_at: now,
            quotes: vec![quote.clone()],
            context: String::new(),
        };
        (snapshot, quote)
    }

    fn buy_signal(
        id: &str,
        symbol: &str,
        quantity: u32,
        entry_price: f64,
        snapshot_id: Option<Uuid>,
    ) -> Signal {
        Signal {
            id: id.into(),
            source: "test".into(),
            symbol: symbol.into(),
            side: Side::Buy,
            quantity,
            entry_price,
            target_price: entry_price * 2.0,
            stop_loss_price: entry_price * 0.5,
            trailing_stop_loss_price: entry_price * 0.8,
            confidence_bps: 8000,
            rationale: "buy test".into(),
            snapshot_id,
        }
    }

    fn sell_signal(
        id: &str,
        symbol: &str,
        quantity: u32,
        entry_price: f64,
        snapshot_id: Option<Uuid>,
    ) -> Signal {
        Signal {
            id: id.into(),
            source: "test".into(),
            symbol: symbol.into(),
            side: Side::Sell,
            quantity,
            entry_price,
            target_price: entry_price * 3.0,
            stop_loss_price: entry_price * 0.3,
            trailing_stop_loss_price: entry_price * 0.6,
            confidence_bps: 8000,
            rationale: "sell test".into(),
            snapshot_id,
        }
    }

    #[test]
    fn buy_then_sell_full_close_round_trip_with_short_disabled_and_max_open_positions() {
        let paper = sample_paper();
        let mut broker = PaperBroker::new(paper.clone());

        let (snap1, q1) = make_snapshot("NIFTY-25000-CE", 100.0, 100.0);
        let buy = buy_signal("b1", "NIFTY-25000-CE", 10, 101.0, Some(snap1.id));
        let fill1 = broker.execute(&buy, &q1, &snap1, 0, 0, 15).unwrap();

        assert_eq!(fill1.quantity, 10);
        assert_eq!(broker.open_position_qty("NIFTY-25000-CE"), 10);
        assert_eq!(broker.open_positions_count(), 1);
        assert!((broker.avg_entry_price("NIFTY-25000-CE").unwrap() - fill1.price).abs() < 1e-9);
        assert_eq!(broker.realized_pnl_paise(), 0);

        // Close at max_open_positions, daily_loss_paise >= max_daily_loss_paise, and appreciated exit
        let (snap2, q2) = make_snapshot("NIFTY-25000-CE", 250.0, 250.0);
        let sell = sell_signal("s1", "NIFTY-25000-CE", 10, 249.0, Some(snap2.id));
        let fill2 = broker
            .execute(
                &sell,
                &q2,
                &snap2,
                paper.max_open_positions,
                paper.max_daily_loss_paise,
                15,
            )
            .unwrap();

        assert_eq!(fill2.quantity, 10);
        assert_eq!(broker.open_position_qty("NIFTY-25000-CE"), 0);
        assert_eq!(broker.open_positions_count(), 0);
        assert_eq!(broker.avg_entry_price("NIFTY-25000-CE"), None);
        assert!(broker.realized_pnl_paise() > 0);
        let cash_delta = broker.cash_paise - paper.starting_capital_paise;
        assert!((broker.realized_pnl_paise() - cash_delta).abs() <= 1);
    }

    #[test]
    fn multiple_buys_weighted_avg_then_partial_and_full_sell_round_trip() {
        let paper = sample_paper();
        let mut broker = PaperBroker::new(paper.clone());

        let (snap1, q1) = make_snapshot("NIFTY-25100-PE", 100.0, 100.0);
        let b1 = buy_signal("b1", "NIFTY-25100-PE", 4, 101.0, Some(snap1.id));
        let f1 = broker.execute(&b1, &q1, &snap1, 0, 0, 15).unwrap();

        let (snap2, q2) = make_snapshot("NIFTY-25100-PE", 130.0, 130.0);
        let b2 = buy_signal("b2", "NIFTY-25100-PE", 6, 131.0, Some(snap2.id));
        let f2 = broker
            .execute(&b2, &q2, &snap2, paper.max_open_positions, 0, 15)
            .unwrap();

        let expected_avg = (4.0 * f1.price + 6.0 * f2.price) / 10.0;
        assert_eq!(broker.open_position_qty("NIFTY-25100-PE"), 10);
        assert_eq!(broker.open_positions_count(), 1);
        assert!((broker.avg_entry_price("NIFTY-25100-PE").unwrap() - expected_avg).abs() < 1e-9);

        // Partial sell of 4 units
        let (snap3, q3) = make_snapshot("NIFTY-25100-PE", 150.0, 150.0);
        let s1 = sell_signal("s1", "NIFTY-25100-PE", 4, 149.0, Some(snap3.id));
        let _f3 = broker.execute(&s1, &q3, &snap3, 1, 0, 15).unwrap();

        assert_eq!(broker.open_position_qty("NIFTY-25100-PE"), 6);
        assert_eq!(broker.open_positions_count(), 1);
        assert!((broker.avg_entry_price("NIFTY-25100-PE").unwrap() - expected_avg).abs() < 1e-9);
        let partial_pnl = broker.realized_pnl_paise();
        assert!(partial_pnl > 0);

        // Final sell of remaining 6 units
        let (snap4, q4) = make_snapshot("NIFTY-25100-PE", 160.0, 160.0);
        let s2 = sell_signal("s2", "NIFTY-25100-PE", 6, 159.0, Some(snap4.id));
        let _f4 = broker.execute(&s2, &q4, &snap4, 1, 0, 15).unwrap();

        assert_eq!(broker.open_position_qty("NIFTY-25100-PE"), 0);
        assert_eq!(broker.open_positions_count(), 0);
        assert_eq!(broker.avg_entry_price("NIFTY-25100-PE"), None);
        assert!(broker.realized_pnl_paise() > partial_pnl);
        let cash_delta = broker.cash_paise - paper.starting_capital_paise;
        assert!((broker.realized_pnl_paise() - cash_delta).abs() <= 2);
    }

    #[test]
    fn partial_fill_fraction_updates_positions_and_pnl_by_filled_qty_only() {
        let mut paper = sample_paper();
        paper.partial_fill_fraction = 0.5;
        let mut broker = PaperBroker::new(paper.clone());

        let (snap1, q1) = make_snapshot("NIFTY-25200-CE", 100.0, 100.0);
        let buy = buy_signal("b1", "NIFTY-25200-CE", 4, 101.0, Some(snap1.id));
        let f1 = broker.execute(&buy, &q1, &snap1, 0, 0, 15).unwrap();
        assert_eq!(f1.quantity, 2);
        assert_eq!(broker.open_position_qty("NIFTY-25200-CE"), 2);

        // Sell request of 2 with partial_fill_fraction = 0.5 fills 1
        let (snap2, q2) = make_snapshot("NIFTY-25200-CE", 120.0, 120.0);
        let sell = sell_signal("s1", "NIFTY-25200-CE", 2, 119.0, Some(snap2.id));
        let f2 = broker.execute(&sell, &q2, &snap2, 1, 0, 15).unwrap();
        assert_eq!(f2.quantity, 1);
        assert_eq!(broker.open_position_qty("NIFTY-25200-CE"), 1);

        // Quantity 1 with partial_fill_fraction = 0.5 yields 0 -> simulated zero fill, no mutation
        let cash_before = broker.cash_paise;
        let pnl_before = broker.realized_pnl_paise();
        let zero_sell = sell_signal("s2", "NIFTY-25200-CE", 1, 119.0, Some(snap2.id));
        let err = broker.execute(&zero_sell, &q2, &snap2, 1, 0, 15).unwrap_err();
        assert!(err.to_string().contains("simulated zero fill"));
        assert_eq!(broker.cash_paise, cash_before);
        assert_eq!(broker.realized_pnl_paise(), pnl_before);
        assert_eq!(broker.open_position_qty("NIFTY-25200-CE"), 1);
    }

    #[test]
    fn sell_without_or_exceeding_position_rejected_without_state_mutation() {
        let paper = sample_paper();
        let mut broker = PaperBroker::new(paper);
        let (snap, q) = make_snapshot("NIFTY-25000-CE", 100.0, 100.0);

        let naked_sell = sell_signal("s0", "NIFTY-25000-CE", 1, 99.0, Some(snap.id));
        let err = broker.execute(&naked_sell, &q, &snap, 0, 0, 15).unwrap_err();
        assert!(err.to_string().contains("short selling disabled"));
        assert_eq!(broker.cash_paise, 1_000_000);
        assert_eq!(broker.open_positions_count(), 0);

        let buy = buy_signal("b1", "NIFTY-25000-CE", 2, 101.0, Some(snap.id));
        broker.execute(&buy, &q, &snap, 0, 0, 15).unwrap();
        let cash_after_buy = broker.cash_paise;

        let excess_sell = sell_signal("s1", "NIFTY-25000-CE", 3, 99.0, Some(snap.id));
        let err = broker.execute(&excess_sell, &q, &snap, 1, 0, 15).unwrap_err();
        assert!(err.to_string().contains("short selling disabled"));
        assert_eq!(broker.cash_paise, cash_after_buy);
        assert_eq!(broker.open_position_qty("NIFTY-25000-CE"), 2);
    }

    #[test]
    fn allow_short_true_blocked_by_long_only_tracker_without_state_mutation() {
        let mut paper = sample_paper();
        paper.allow_short = true;
        let mut broker = PaperBroker::new(paper);
        let (snap, q) = make_snapshot("NIFTY-25000-CE", 100.0, 100.0);

        let short_sell = sell_signal("s1", "NIFTY-25000-CE", 2, 99.0, Some(snap.id));
        let err = broker.execute(&short_sell, &q, &snap, 0, 0, 15).unwrap_err();
        assert!(err
            .to_string()
            .contains("short positions are not supported by PaperBroker long-only tracker"));
        assert_eq!(broker.cash_paise, 1_000_000);
        assert_eq!(broker.open_positions_count(), 0);
        assert_eq!(broker.realized_pnl_paise(), 0);
    }

    #[test]
    fn limit_and_plan_bound_rejections_do_not_mutate_state() {
        let paper = sample_paper();
        let mut broker = PaperBroker::new(paper);
        let (snap, q) = make_snapshot("NIFTY-25000-CE", 100.0, 100.0);

        // Buy simulated px = 100.1 > entry_price 100.0 -> rejected
        let buy_limit_fail = buy_signal("b_lim", "NIFTY-25000-CE", 2, 100.0, Some(snap.id));
        let err = broker
            .execute(&buy_limit_fail, &q, &snap, 0, 0, 15)
            .unwrap_err();
        assert!(err.to_string().contains("simulated price exceeds signal limit"));
        assert_eq!(broker.cash_paise, 1_000_000);
        assert_eq!(broker.open_positions_count(), 0);

        // Buy simulated px = 100.1 >= target_price 100.05 -> plan-bounds rejected
        let mut buy_plan_fail = buy_signal("b_plan", "NIFTY-25000-CE", 2, 100.02, Some(snap.id));
        buy_plan_fail.target_price = 100.05;
        let err = broker
            .execute(&buy_plan_fail, &q, &snap, 0, 0, 15)
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("simulated entry falls outside the agent-selected trade plan"));
        assert_eq!(broker.cash_paise, 1_000_000);
        assert_eq!(broker.open_positions_count(), 0);

        // Open valid buy first, then test Sell limit rejection (px = 99.9 < entry_price 100.0)
        let buy_ok = buy_signal("b_ok", "NIFTY-25000-CE", 2, 100.5, Some(snap.id));
        broker.execute(&buy_ok, &q, &snap, 0, 0, 15).unwrap();
        let cash_after_buy = broker.cash_paise;
        let avg_after_buy = broker.avg_entry_price("NIFTY-25000-CE");

        let sell_limit_fail = sell_signal("s_lim", "NIFTY-25000-CE", 2, 100.0, Some(snap.id));
        let err = broker
            .execute(&sell_limit_fail, &q, &snap, 1, 0, 15)
            .unwrap_err();
        assert!(err.to_string().contains("simulated price exceeds signal limit"));
        assert_eq!(broker.cash_paise, cash_after_buy);
        assert_eq!(broker.open_position_qty("NIFTY-25000-CE"), 2);
        assert_eq!(broker.avg_entry_price("NIFTY-25000-CE"), avg_after_buy);
        assert_eq!(broker.realized_pnl_paise(), 0);
    }

    #[test]
    fn snapshot_mismatch_and_external_stale_quote_rejected() {
        let paper = sample_paper();
        let mut broker = PaperBroker::new(paper);
        let (snap, _q) = make_snapshot("NIFTY-25000-CE", 100.0, 100.0);

        let wrong_snap_sig =
            buy_signal("b1", "NIFTY-25000-CE", 1, 101.0, Some(Uuid::new_v4()));
        let err = broker
            .execute(&wrong_snap_sig, &snap.quotes[0], &snap, 0, 0, 15)
            .unwrap_err();
        assert!(err.to_string().contains("signal snapshot mismatch"));

        let mut stale_quote = snap.quotes[0].clone();
        stale_quote.timestamp = Utc::now() - Duration::seconds(60);
        let ok_sig = buy_signal("b2", "NIFTY-25000-CE", 1, 101.0, Some(snap.id));
        let err = broker
            .execute(&ok_sig, &stale_quote, &snap, 0, 0, 15)
            .unwrap_err();
        assert!(err.to_string().contains("stale market data"));
        assert_eq!(broker.open_positions_count(), 0);
    }

    #[test]
    fn effective_open_positions_enforces_both_internal_and_external_counts() {
        let mut paper = sample_paper();
        paper.max_open_positions = 2;
        let mut broker = PaperBroker::new(paper);

        let (snap1, q1) = make_snapshot("SYM-A", 100.0, 100.0);
        let (snap2, q2) = make_snapshot("SYM-B", 100.0, 100.0);
        let (snap3, q3) = make_snapshot("SYM-C", 100.0, 100.0);

        // External open_positions = 2 blocks opening SYM-A even on empty broker
        let b_a = buy_signal("ba", "SYM-A", 1, 101.0, Some(snap1.id));
        assert!(broker
            .execute(&b_a, &q1, &snap1, 2, 0, 15)
            .unwrap_err()
            .to_string()
            .contains("open position limit"));

        // Open SYM-A and SYM-B
        broker.execute(&b_a, &q1, &snap1, 0, 0, 15).unwrap();
        let b_b = buy_signal("bb", "SYM-B", 1, 101.0, Some(snap2.id));
        broker.execute(&b_b, &q2, &snap2, 0, 0, 15).unwrap();
        assert_eq!(broker.open_positions_count(), 2);

        // Passing external open_positions = 0 still blocks SYM-C via internal count
        let b_c = buy_signal("bc", "SYM-C", 1, 101.0, Some(snap3.id));
        assert!(broker
            .execute(&b_c, &q3, &snap3, 0, 0, 15)
            .unwrap_err()
            .to_string()
            .contains("open position limit"));
    }

    #[test]
    fn buy_u32_max_then_buy_one_returns_err_without_panic_and_preserves_state() {
        let mut paper = sample_paper();
        paper.max_position_quantity = u32::MAX;
        paper.max_trade_value_paise = i64::MAX;
        paper.starting_capital_paise = i64::MAX;
        let mut broker = PaperBroker::new(paper);

        let (snap1, q1) = make_snapshot("NIFTY-25000-CE", 0.0099, 0.0099);
        let buy_max = buy_signal("b_max", "NIFTY-25000-CE", u32::MAX, 0.01, Some(snap1.id));
        let fill1 = broker.execute(&buy_max, &q1, &snap1, 0, 0, 15).unwrap();
        assert_eq!(fill1.quantity, u32::MAX);
        assert_eq!(broker.open_position_qty("NIFTY-25000-CE"), u32::MAX);
        let cash_after_max = broker.cash_paise;

        let (snap2, q2) = make_snapshot("NIFTY-25000-CE", 0.0099, 0.0099);
        let buy_one = buy_signal("b_one", "NIFTY-25000-CE", 1, 0.01, Some(snap2.id));
        let err = broker.execute(&buy_one, &q2, &snap2, 1, 0, 15).unwrap_err();
        assert!(err.to_string().contains("quantity limit"));
        assert_eq!(broker.open_position_qty("NIFTY-25000-CE"), u32::MAX);
        assert_eq!(broker.cash_paise, cash_after_max);
    }

    #[test]
    fn sell_cash_overflow_returns_err_atomically_without_panic() {
        let mut paper = sample_paper();
        paper.starting_capital_paise = i64::MAX;
        paper.max_trade_value_paise = i64::MAX;
        let mut broker = PaperBroker::new(paper);

        let (snap1, q1) = make_snapshot("NIFTY-25000-CE", 1.0, 1.0);
        let buy = buy_signal("b1", "NIFTY-25000-CE", 1, 1.1, Some(snap1.id));
        broker.execute(&buy, &q1, &snap1, 0, 0, 15).unwrap();
        let cash_after_buy = broker.cash_paise;
        let avg_after_buy = broker.avg_entry_price("NIFTY-25000-CE");

        // Appreciated sell whose proceeds exceed remaining i64::MAX headroom
        let (snap2, q2) = make_snapshot("NIFTY-25000-CE", 1_000_000.0, 1_000_000.0);
        let sell = sell_signal("s1", "NIFTY-25000-CE", 1, 900_000.0, Some(snap2.id));
        let err = broker.execute(&sell, &q2, &snap2, 1, 0, 15).unwrap_err();
        assert!(err.to_string().contains("overflow"));
        assert_eq!(broker.cash_paise, cash_after_buy);
        assert_eq!(broker.open_position_qty("NIFTY-25000-CE"), 1);
        assert_eq!(broker.avg_entry_price("NIFTY-25000-CE"), avg_after_buy);
        assert_eq!(broker.realized_pnl_paise(), 0);
    }

    #[test]
    fn checked_rounded_i64_rejects_exact_2_pow_63_and_accepts_representable_bounds() {
        assert!(checked_rounded_i64(2.0f64.powi(63), "overflow").is_err());
        assert!(checked_rounded_i64(2.0f64.powi(63) - 1024.0, "overflow").is_ok());
        assert_eq!(
            checked_rounded_i64(i64::MIN as f64, "underflow").unwrap(),
            i64::MIN
        );
        assert!(checked_rounded_i64((i64::MIN as f64) - 2048.0, "underflow").is_err());
    }

    #[test]
    fn buy_at_exact_2_pow_63_with_cash_i64_max_returns_err_without_mutation() {
        let mut paper = sample_paper();
        paper.max_position_quantity = u32::MAX;
        paper.max_trade_value_paise = i64::MAX;
        paper.starting_capital_paise = i64::MAX;
        paper.spread_bps = 0;
        paper.slippage_bps = 0;
        paper.fee_bps = 0;
        let mut broker = PaperBroker::new(paper);

        let (snap1, q1) = make_snapshot("SYM-2P63", 42_949_672.96, 42_949_672.96);
        let buy_exact = buy_signal("b_2p63", "SYM-2P63", 1 << 31, 42_949_672.96, Some(snap1.id));
        let err = broker
            .execute(&buy_exact, &q1, &snap1, 0, 0, 15)
            .unwrap_err();
        assert!(err.to_string().contains("per-trade capital limit"));
        assert_eq!(broker.cash_paise, i64::MAX);
        assert_eq!(broker.open_positions_count(), 0);
        assert_eq!(broker.open_position_qty("SYM-2P63"), 0);

        // Also exercise PaperBroker::execute's post-fee gross check when pre-fee value <= i64::MAX
        // but fees push gross >= 2.0f64.powi(63) against cash_paise == i64::MAX.
        broker.config.fee_bps = 1;
        let (snap2, q2) = make_snapshot("SYM-2P63", 42_949_672.0, 42_949_672.0);
        let buy_fee_push = buy_signal("b_fee", "SYM-2P63", 1 << 31, 42_949_672.0, Some(snap2.id));
        let err = broker
            .execute(&buy_fee_push, &q2, &snap2, 0, 0, 15)
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("insufficient cash after simulated costs"));
        assert_eq!(broker.cash_paise, i64::MAX);
        assert_eq!(broker.open_positions_count(), 0);
        assert_eq!(broker.open_position_qty("SYM-2P63"), 0);
    }

    #[test]
    fn sell_at_exact_2_pow_63_with_cash_zero_returns_err_without_mutation() {
        let mut paper = sample_paper();
        paper.max_position_quantity = u32::MAX;
        paper.max_trade_value_paise = i64::MAX;
        // Buy (1 << 31) units at 0.01 rupees (1 paise/unit) so cash_paise becomes exactly 0.
        paper.starting_capital_paise = 1_i64 << 31;
        paper.spread_bps = 0;
        paper.slippage_bps = 0;
        paper.fee_bps = 0;
        let mut broker = PaperBroker::new(paper);

        let (snap1, q1) = make_snapshot("SYM-ZC", 0.01, 0.01);
        let buy = buy_signal("b_open", "SYM-ZC", 1 << 31, 0.01, Some(snap1.id));
        broker.execute(&buy, &q1, &snap1, 0, 0, 15).unwrap();
        assert_eq!(broker.cash_paise, 0);
        assert_eq!(broker.open_position_qty("SYM-ZC"), 1 << 31);
        let avg_before = broker.avg_entry_price("SYM-ZC");

        let (snap2, q2) = make_snapshot("SYM-ZC", 42_949_672.96, 42_949_672.96);
        let sell = sell_signal("s_2p63", "SYM-ZC", 1 << 31, 42_949_672.96, Some(snap2.id));
        let err = broker.execute(&sell, &q2, &snap2, 1, 0, 15).unwrap_err();
        assert!(err
            .to_string()
            .contains("simulated execution cash overflow"));
        assert_eq!(broker.cash_paise, 0);
        assert_eq!(broker.open_position_qty("SYM-ZC"), 1 << 31);
        assert_eq!(broker.avg_entry_price("SYM-ZC"), avg_before);
        assert_eq!(broker.realized_pnl_paise(), 0);
    }

    #[test]
    fn execute_at_uses_injected_clock_and_tracks_open_positions() {
        use chrono::TimeZone;
        let sim_now = Utc.with_ymd_and_hms(2025, 1, 15, 4, 0, 0).unwrap();
        let paper = sample_paper();
        let mut broker = PaperBroker::new(paper);

        let q = Quote {
            symbol: "NIFTY-25000-CE".into(),
            bid: 100.0,
            ask: 100.5,
            last: 100.25,
            timestamp: sim_now,
        };
        let snap = MarketSnapshot {
            id: Uuid::new_v4(),
            created_at: sim_now,
            quotes: vec![q.clone()],
            context: String::new(),
        };
        let buy = buy_signal("b_sim", "NIFTY-25000-CE", 3, 102.0, Some(snap.id));
        let fill = broker
            .execute_at(&buy, &q, &snap, 0, 0, 15, sim_now)
            .unwrap();
        assert_eq!(fill.timestamp, sim_now);
        assert_eq!(broker.open_symbols(), vec!["NIFTY-25000-CE".to_string()]);
        assert_eq!(
            broker.open_positions(),
            vec![("NIFTY-25000-CE".to_string(), 3)]
        );

        // Stale quote relative to sim_now + 20s rejected
        let later = sim_now + Duration::seconds(20);
        let buy2 = buy_signal("b_stale", "NIFTY-25000-CE", 1, 102.0, Some(snap.id));
        let err = broker
            .execute_at(&buy2, &q, &snap, 1, 0, 15, later)
            .unwrap_err();
        assert!(err.to_string().contains("stale"));
    }
}

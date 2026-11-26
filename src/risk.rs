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
    check_signal_with_open_qty(
        signal,
        quote,
        paper,
        cash_paise,
        open_positions,
        daily_loss_paise,
        0,
    )
}

pub fn check_signal_with_open_qty(
    signal: &Signal,
    quote: &Quote,
    paper: &Paper,
    cash_paise: i64,
    open_positions: usize,
    daily_loss_paise: i64,
    open_symbol_qty: u32,
) -> Result<()> {
    check_agent_plan(signal)?;
    if signal.id.trim().is_empty()
        || signal.symbol.trim().is_empty()
        || signal.symbol != quote.symbol
    {
        bail!("signal identity/symbol invalid")
    }
    if signal.quantity == 0 {
        bail!("quantity limit")
    }
    if signal.confidence_bps > 10_000 {
        bail!("confidence out of range")
    }
    let value = signal.entry_price * signal.quantity as f64 * 100.0;
    if !value.is_finite() {
        bail!("per-trade capital limit")
    }

    match signal.side {
        Side::Sell => {
            if open_symbol_qty >= signal.quantity {
                // Closing an existing long position reduces risk and is permitted even when
                // short selling is disabled, open_positions >= max_open_positions,
                // daily_loss_paise >= max_daily_loss_paise, or appreciated value exceeds
                // max_trade_value_paise.
                return Ok(());
            }
            if !paper.allow_short {
                bail!("short selling disabled")
            }
            if signal.quantity > paper.max_position_quantity {
                bail!("quantity limit")
            }
            if daily_loss_paise >= paper.max_daily_loss_paise {
                bail!("daily loss limit")
            }
            if (open_symbol_qty == 0 && open_positions >= paper.max_open_positions)
                || open_positions > paper.max_open_positions
            {
                bail!("open position limit")
            }
            if exceeds_i64(value, paper.max_trade_value_paise) {
                bail!("per-trade capital limit")
            }
        }
        Side::Buy => {
            if open_symbol_qty
                .checked_add(signal.quantity)
                .is_none_or(|total| total > paper.max_position_quantity)
            {
                bail!("quantity limit")
            }
            if daily_loss_paise >= paper.max_daily_loss_paise {
                bail!("daily loss limit")
            }
            if (open_symbol_qty == 0 && open_positions >= paper.max_open_positions)
                || open_positions > paper.max_open_positions
            {
                bail!("open position limit")
            }
            if exceeds_i64(value, paper.max_trade_value_paise) {
                bail!("per-trade capital limit")
            }
            if cash_paise <= 0 || exceeds_i64(value, cash_paise) {
                bail!("insufficient paper capital")
            }
        }
    }

    Ok(())
}

/// Checks agent-selected prices without deriving or modifying any level.
pub fn check_agent_plan(signal: &Signal) -> Result<()> {
    if !signal.entry_price.is_finite() || signal.entry_price <= 0.0 {
        bail!("invalid agent entry price")
    }
    if signal.side == Side::Buy
        && (!signal.target_price.is_finite()
            || !signal.stop_loss_price.is_finite()
            || !signal.trailing_stop_loss_price.is_finite()
            || signal.stop_loss_price <= 0.0
            || signal.stop_loss_price >= signal.trailing_stop_loss_price
            || signal.trailing_stop_loss_price > signal.entry_price
            || signal.entry_price >= signal.target_price)
    {
        bail!("agent trade plan levels invalid; require stop < trail <= entry < target")
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

pub(crate) fn exceeds_i64(val: f64, limit: i64) -> bool {
    if !val.is_finite() || val >= -(i64::MIN as f64) {
        return true;
    }
    if val < i64::MIN as f64 {
        return false;
    }
    let trunc = val.trunc() as i64;
    trunc > limit || (trunc == limit && val.fract() > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
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

    fn sample_quote() -> Quote {
        Quote {
            symbol: "NIFTY-25000-CE".into(),
            bid: 100.0,
            ask: 100.2,
            last: 100.1,
            timestamp: Utc::now(),
        }
    }

    fn sample_signal(side: Side, quantity: u32, entry_price: f64) -> Signal {
        Signal {
            id: "sig-1".into(),
            source: "test".into(),
            symbol: "NIFTY-25000-CE".into(),
            side,
            quantity,
            entry_price,
            target_price: entry_price * 1.25,
            stop_loss_price: entry_price * 0.8,
            trailing_stop_loss_price: entry_price * 0.9,
            confidence_bps: 7500,
            rationale: "test rationale".into(),
            snapshot_id: None,
        }
    }

    #[test]
    fn buy_valid_signal_passes() {
        let paper = sample_paper();
        let quote = sample_quote();
        let signal = sample_signal(Side::Buy, 10, 100.2);
        assert!(check_signal(&signal, &quote, &paper, 1_000_000, 0, 0).is_ok());
        assert!(check_signal_with_open_qty(&signal, &quote, &paper, 1_000_000, 0, 0, 0).is_ok());
    }

    #[test]
    fn check_signal_delegates_with_zero_open_qty_and_blocks_sell_when_short_disabled() {
        let paper = sample_paper();
        let quote = sample_quote();
        let signal = sample_signal(Side::Sell, 5, 100.0);

        let err1 = check_signal(&signal, &quote, &paper, 1_000_000, 0, 0).unwrap_err();
        let err2 =
            check_signal_with_open_qty(&signal, &quote, &paper, 1_000_000, 0, 0, 0).unwrap_err();
        assert!(err1.to_string().contains("short selling disabled"));
        assert!(err2.to_string().contains("short selling disabled"));
    }

    #[test]
    fn sell_exceeding_open_qty_rejected_when_short_disabled() {
        let paper = sample_paper();
        let quote = sample_quote();
        let signal = sample_signal(Side::Sell, 5, 100.0);

        let err =
            check_signal_with_open_qty(&signal, &quote, &paper, 1_000_000, 1, 0, 2).unwrap_err();
        assert!(err.to_string().contains("short selling disabled"));
    }

    #[test]
    fn sell_closing_long_allowed_when_short_disabled_and_at_limits() {
        let paper = sample_paper();
        let quote = sample_quote();
        // Appreciated value: 250.0 * 10 * 100 = 250_000 > max_trade_value_paise (200_000)
        let signal = sample_signal(Side::Sell, 10, 250.0);

        // Full close at max_open_positions, at daily loss limit, with low cash & appreciated value
        assert!(check_signal_with_open_qty(
            &signal,
            &quote,
            &paper,
            0,
            paper.max_open_positions,
            paper.max_daily_loss_paise,
            10,
        )
        .is_ok());

        // Over max_open_positions is also allowed for risk-reducing close
        assert!(check_signal_with_open_qty(
            &signal,
            &quote,
            &paper,
            0,
            paper.max_open_positions + 1,
            paper.max_daily_loss_paise + 10_000,
            10,
        )
        .is_ok());
    }

    #[test]
    fn sell_partial_close_allowed_when_short_disabled() {
        let paper = sample_paper();
        let quote = sample_quote();
        let signal = sample_signal(Side::Sell, 4, 100.0);

        assert!(check_signal_with_open_qty(
            &signal,
            &quote,
            &paper,
            100,
            paper.max_open_positions,
            paper.max_daily_loss_paise,
            10,
        )
        .is_ok());
    }

    #[test]
    fn sell_short_allowed_when_allow_short_true_and_enforces_limits() {
        let mut paper = sample_paper();
        paper.allow_short = true;
        let quote = sample_quote();

        let ok_sig = sample_signal(Side::Sell, 10, 100.0);
        assert!(
            check_signal_with_open_qty(&ok_sig, &quote, &paper, 1_000_000, 0, 0, 0).is_ok()
        );

        // Exceeds max_position_quantity
        let big_qty = sample_signal(Side::Sell, 51, 10.0);
        assert!(
            check_signal_with_open_qty(&big_qty, &quote, &paper, 1_000_000, 0, 0, 0)
                .unwrap_err()
                .to_string()
                .contains("quantity limit")
        );

        // Daily loss limit
        assert!(check_signal_with_open_qty(
            &ok_sig,
            &quote,
            &paper,
            1_000_000,
            0,
            paper.max_daily_loss_paise,
            0
        )
        .unwrap_err()
        .to_string()
        .contains("daily loss limit"));

        // Open position limit when open_symbol_qty == 0
        assert!(check_signal_with_open_qty(
            &ok_sig,
            &quote,
            &paper,
            1_000_000,
            paper.max_open_positions,
            0,
            0
        )
        .unwrap_err()
        .to_string()
        .contains("open position limit"));

        // Flipping existing symbol at max_open_positions is permitted within policy
        assert!(check_signal_with_open_qty(
            &ok_sig,
            &quote,
            &paper,
            1_000_000,
            paper.max_open_positions,
            0,
            5
        )
        .is_ok());

        // Exceeding max_open_positions (> cap) still fails even with open_symbol_qty > 0
        assert!(check_signal_with_open_qty(
            &ok_sig,
            &quote,
            &paper,
            1_000_000,
            paper.max_open_positions + 1,
            0,
            5
        )
        .unwrap_err()
        .to_string()
        .contains("open position limit"));

        // Per-trade capital limit
        let too_valuable = sample_signal(Side::Sell, 25, 100.0);
        assert!(check_signal_with_open_qty(
            &too_valuable,
            &quote,
            &paper,
            1_000_000,
            0,
            0,
            0
        )
        .unwrap_err()
        .to_string()
        .contains("per-trade capital limit"));
    }

    #[test]
    fn buy_adding_to_existing_long_policy_and_caps() {
        let paper = sample_paper();
        let quote = sample_quote();
        let sig = sample_signal(Side::Buy, 10, 100.0);

        // Adding to existing open symbol at max_open_positions succeeds within qty & trade value
        assert!(check_signal_with_open_qty(
            &sig,
            &quote,
            &paper,
            1_000_000,
            paper.max_open_positions,
            0,
            40,
        )
        .is_ok());

        // Cumulative open_symbol_qty + signal.quantity > max_position_quantity fails
        let err = check_signal_with_open_qty(
            &sig,
            &quote,
            &paper,
            1_000_000,
            paper.max_open_positions,
            0,
            41,
        )
        .unwrap_err();
        assert!(err.to_string().contains("quantity limit"));

        // Over max_open_positions (> cap) fails even when adding
        let err = check_signal_with_open_qty(
            &sig,
            &quote,
            &paper,
            1_000_000,
            paper.max_open_positions + 1,
            0,
            10,
        )
        .unwrap_err();
        assert!(err.to_string().contains("open position limit"));
    }

    #[test]
    fn buy_rejects_out_of_policy_limits() {
        let paper = sample_paper();
        let quote = sample_quote();
        let sig = sample_signal(Side::Buy, 10, 100.0); // value = 100_000

        assert!(check_signal(&sig, &quote, &paper, 1_000_000, 0, paper.max_daily_loss_paise)
            .unwrap_err()
            .to_string()
            .contains("daily loss limit"));

        assert!(check_signal(&sig, &quote, &paper, 1_000_000, paper.max_open_positions, 0)
            .unwrap_err()
            .to_string()
            .contains("open position limit"));

        let over_value = sample_signal(Side::Buy, 25, 100.0); // value = 250_000
        assert!(check_signal(&over_value, &quote, &paper, 1_000_000, 0, 0)
            .unwrap_err()
            .to_string()
            .contains("per-trade capital limit"));

        assert!(check_signal(&sig, &quote, &paper, 99_999, 0, 0)
            .unwrap_err()
            .to_string()
            .contains("insufficient paper capital"));

        assert!(check_signal(&sig, &quote, &paper, 0, 0, 0)
            .unwrap_err()
            .to_string()
            .contains("insufficient paper capital"));
    }

    #[test]
    fn rejects_structural_invalids_on_all_sides() {
        let paper = sample_paper();
        let quote = sample_quote();

        let mut bad_id = sample_signal(Side::Buy, 1, 100.0);
        bad_id.id = "   ".into();
        assert!(check_signal(&bad_id, &quote, &paper, 1_000_000, 0, 0).is_err());

        let mut empty_sym = sample_signal(Side::Sell, 1, 100.0);
        empty_sym.symbol = " ".into();
        let mut empty_quote = quote.clone();
        empty_quote.symbol = " ".into();
        assert!(check_signal_with_open_qty(
            &empty_sym,
            &empty_quote,
            &paper,
            1_000_000,
            0,
            0,
            10
        )
        .is_err());

        let mut mismatch = sample_signal(Side::Sell, 1, 100.0);
        mismatch.symbol = "OTHER".into();
        assert!(
            check_signal_with_open_qty(&mismatch, &quote, &paper, 1_000_000, 0, 0, 10).is_err()
        );

        let zero_qty = sample_signal(Side::Sell, 0, 100.0);
        assert!(
            check_signal_with_open_qty(&zero_qty, &quote, &paper, 1_000_000, 0, 0, 10).is_err()
        );

        let mut bad_conf = sample_signal(Side::Sell, 1, 100.0);
        bad_conf.confidence_bps = 10_001;
        assert!(
            check_signal_with_open_qty(&bad_conf, &quote, &paper, 1_000_000, 0, 0, 10).is_err()
        );

        let overflow_val = sample_signal(Side::Sell, 10, f64::MAX);
        assert!(
            check_signal_with_open_qty(&overflow_val, &quote, &paper, 1_000_000, 0, 0, 10)
                .unwrap_err()
                .to_string()
                .contains("per-trade capital limit")
        );
    }

    #[test]
    fn check_agent_plan_preserves_buy_level_checks_and_sell_flexibility() {
        let mut bad_entry = sample_signal(Side::Buy, 1, 0.0);
        assert!(check_agent_plan(&bad_entry).is_err());
        bad_entry.entry_price = f64::NAN;
        assert!(check_agent_plan(&bad_entry).is_err());

        let mut bad_buy = sample_signal(Side::Buy, 1, 100.0);
        bad_buy.trailing_stop_loss_price = 105.0; // trail > entry
        assert!(check_agent_plan(&bad_buy).is_err());

        let mut bad_target = sample_signal(Side::Buy, 1, 100.0);
        bad_target.target_price = 100.0; // target <= entry
        assert!(check_agent_plan(&bad_target).is_err());

        // RoundTripPolicy style Sell with target > entry and stop < entry must stay valid
        let sell = sample_signal(Side::Sell, 1, 100.0);
        assert!(check_agent_plan(&sell).is_ok());
    }

    #[test]
    fn check_market_snapshot_rejects_empty_stale_and_invalid_quotes() {
        let empty = MarketSnapshot {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            quotes: vec![],
            context: String::new(),
        };
        assert!(check_market_snapshot(&empty, 15).is_err());

        let stale_snap = MarketSnapshot {
            id: Uuid::new_v4(),
            created_at: Utc::now() - Duration::seconds(30),
            quotes: vec![sample_quote()],
            context: String::new(),
        };
        assert!(check_market_snapshot(&stale_snap, 15).is_err());

        let mut bad_quote = sample_quote();
        bad_quote.ask = 90.0; // ask < bid
        let bad_quote_snap = MarketSnapshot {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            quotes: vec![bad_quote],
            context: String::new(),
        };
        assert!(check_market_snapshot(&bad_quote_snap, 15).is_err());
    }

    #[test]
    fn buy_u32_max_overflow_rejected_with_quantity_limit() {
        let mut paper = sample_paper();
        paper.max_position_quantity = u32::MAX;
        paper.max_trade_value_paise = i64::MAX;
        let quote = Quote {
            symbol: "NIFTY-25000-CE".into(),
            bid: 0.01,
            ask: 0.01,
            last: 0.01,
            timestamp: Utc::now(),
        };
        let sig = sample_signal(Side::Buy, 1, 0.01);

        let err =
            check_signal_with_open_qty(&sig, &quote, &paper, i64::MAX, 1, 0, u32::MAX).unwrap_err();
        assert!(err.to_string().contains("quantity limit"));
    }

    #[test]
    fn exceeds_i64_handles_exact_ieee754_and_fractional_boundaries() {
        assert!(exceeds_i64(2.0f64.powi(63), i64::MAX));
        assert!(exceeds_i64(100.5, 100));
        assert!(!exceeds_i64(100.0, 100));
        assert!(!exceeds_i64(-0.5, 0));
        assert!(exceeds_i64(-0.5, -1));
        assert!(!exceeds_i64(-1.5, -1));
        assert!(exceeds_i64(f64::NAN, i64::MAX));
        assert!(exceeds_i64(f64::INFINITY, i64::MAX));
        assert!(exceeds_i64(f64::NEG_INFINITY, i64::MIN));
        assert!(!exceeds_i64(i64::MIN as f64, i64::MIN));
        assert!(!exceeds_i64((i64::MIN as f64) - 2048.0, i64::MIN));
        assert!(!exceeds_i64(2.0f64.powi(63) - 1024.0, i64::MAX));
    }

    #[test]
    fn buy_and_short_sell_at_exact_2_pow_63_rejected_even_with_i64_max_limits() {
        let mut paper = sample_paper();
        paper.max_position_quantity = u32::MAX;
        paper.max_trade_value_paise = i64::MAX;
        let quote = Quote {
            symbol: "NIFTY-25000-CE".into(),
            bid: 42_949_672.96,
            ask: 42_949_672.96,
            last: 42_949_672.96,
            timestamp: Utc::now(),
        };
        // 42_949_672.96 * (1 << 31) * 100.0 == 2.0f64.powi(63)
        let buy = sample_signal(Side::Buy, 1 << 31, 42_949_672.96);
        assert_eq!(
            buy.entry_price * buy.quantity as f64 * 100.0,
            2.0f64.powi(63)
        );
        let err = check_signal_with_open_qty(&buy, &quote, &paper, i64::MAX, 0, 0, 0).unwrap_err();
        assert!(err.to_string().contains("per-trade capital limit"));

        paper.allow_short = true;
        let sell = sample_signal(Side::Sell, 1 << 31, 42_949_672.96);
        let err = check_signal_with_open_qty(&sell, &quote, &paper, i64::MAX, 0, 0, 0).unwrap_err();
        assert!(err.to_string().contains("per-trade capital limit"));

        // Fractional paise over integer cash limit is also rejected
        let mut frac_paper = sample_paper();
        frac_paper.max_trade_value_paise = 200_000;
        let frac_sig = sample_signal(Side::Buy, 1, 100.005); // value = 10_000.5 paise
        let err = check_signal_with_open_qty(&frac_sig, &quote, &frac_paper, 10_000, 0, 0, 0)
            .unwrap_err();
        assert!(err.to_string().contains("insufficient paper capital"));
    }
}

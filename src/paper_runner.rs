use crate::{
    agents::TradingAgentsOrchestrator,
    clock::{ist_date, ist_offset, market_hours_status, to_ist, Clock, MarketHoursStatus},
    config::{Paper, Settings},
    data::MarketDataFeed,
    journal::Journal,
    paper_broker::PaperBroker,
    risk,
    strategy::IndependentStrategy,
    types::{Fill, MarketSnapshot, PipelineTrace, PositionContext, Quote, Side, Signal, TradeOutcome},
    LIVE_ORDER_PLACEMENT_IMPLEMENTED,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, future::Future, pin::Pin, sync::Arc, time::Duration};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Clone, Debug)]
struct TrackedPlan {
    entry_price: f64,
    target_price: f64,
    stop_loss_price: f64,
    trailing_stop_loss_price: f64,
    trailing_distance: f64,
    highest_price: f64,
    entry_time: DateTime<Utc>,
    situation_summary: String,
    decision_rationale: String,
}

#[derive(Debug)]
pub struct PaperStepResult {
    pub trace: PipelineTrace,
    pub fill: Option<Fill>,
    pub reflection_recorded: bool,
    pub duplicate_signal_skipped: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TradeSummary {
    pub trade_id: String,
    pub symbol: String,
    pub quantity: u32,
    pub entry_price: f64,
    pub exit_price: f64,
    pub fees_rupees: f64,
    pub realized_pnl_paise: i64,
    pub realized_pnl_rupees: f64,
    pub return_bps: i32,
    pub exit_reason: String,
    pub entry_time: DateTime<Utc>,
    pub exit_time: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DailySummary {
    pub date_ist: String,
    pub starting_cash_paise: i64,
    pub ending_cash_paise: i64,
    pub realized_pnl_paise: i64,
    pub total_trades: usize,
    pub closed_trades: Vec<TradeSummary>,
    pub eod_squared_off: usize,
    pub open_positions_remaining: usize,
}

impl DailySummary {
    pub fn format_summary(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for DailySummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "=== Paper Trading Daily Summary ({} IST) ===",
            self.date_ist
        )?;
        writeln!(
            f,
            "Starting Cash: ₹{:.2} ({} paise)",
            self.starting_cash_paise as f64 / 100.0,
            self.starting_cash_paise
        )?;
        writeln!(
            f,
            "Ending Cash:   ₹{:.2} ({} paise)",
            self.ending_cash_paise as f64 / 100.0,
            self.ending_cash_paise
        )?;
        writeln!(
            f,
            "Realized P&L:  ₹{:.2} ({} paise)",
            self.realized_pnl_paise as f64 / 100.0,
            self.realized_pnl_paise
        )?;
        writeln!(
            f,
            "Trades Executed (Closed): {} | EOD Squared Off: {} | Open Remaining: {}",
            self.total_trades, self.eod_squared_off, self.open_positions_remaining
        )?;
        if self.closed_trades.is_empty() {
            writeln!(f, "Closed Trades: (none)")?;
        } else {
            writeln!(f, "Closed Trades:")?;
            for t in &self.closed_trades {
                writeln!(
                    f,
                    "  - {} | qty={} | entry={:.2} | exit={:.2} | fees=₹{:.2} | pnl=₹{:.2} ({} bps) | reason={} | id={}",
                    t.symbol,
                    t.quantity,
                    t.entry_price,
                    t.exit_price,
                    t.fees_rupees,
                    t.realized_pnl_rupees,
                    t.return_bps,
                    t.exit_reason,
                    t.trade_id
                )?;
            }
        }
        Ok(())
    }
}

pub struct PaperRunner {
    pub settings: Settings,
    pub orchestrator: TradingAgentsOrchestrator,
    pub broker: PaperBroker,
    pub journal: Arc<Journal>,
    pub strategy: Option<Arc<dyn IndependentStrategy>>,
    plans: BTreeMap<String, TrackedPlan>,
    daily_loss_paise: i64,
    starting_cash_paise: i64,
    closed_trades: Vec<TradeSummary>,
    last_quotes: BTreeMap<String, Quote>,
    eod_squared_off_count: usize,
    pending_reflections: Vec<TradeOutcome>,
}

impl PaperRunner {
    pub fn new(
        settings: Settings,
        orchestrator: TradingAgentsOrchestrator,
        journal: Arc<Journal>,
    ) -> Result<Self> {
        if LIVE_ORDER_PLACEMENT_IMPLEMENTED {
            bail!("live order placement must remain disabled");
        }
        let broker = PaperBroker::new(settings.paper.clone());
        let starting_cash_paise = broker.cash_paise;
        Ok(Self {
            settings,
            orchestrator,
            broker,
            journal,
            strategy: None,
            plans: BTreeMap::new(),
            daily_loss_paise: 0,
            starting_cash_paise,
            closed_trades: Vec::new(),
            last_quotes: BTreeMap::new(),
            eod_squared_off_count: 0,
            pending_reflections: Vec::new(),
        })
    }

    pub fn with_strategy(mut self, strategy: Arc<dyn IndependentStrategy>) -> Self {
        self.strategy = Some(strategy);
        self
    }

    pub fn daily_loss_paise(&self) -> i64 {
        self.daily_loss_paise
    }

    pub fn closed_trades(&self) -> &[TradeSummary] {
        &self.closed_trades
    }

    pub fn daily_summary(&self, now: DateTime<Utc>) -> DailySummary {
        DailySummary {
            date_ist: ist_date(now).format("%Y-%m-%d").to_string(),
            starting_cash_paise: self.starting_cash_paise,
            ending_cash_paise: self.broker.cash_paise,
            realized_pnl_paise: self.broker.realized_pnl_paise(),
            total_trades: self.closed_trades.len(),
            closed_trades: self.closed_trades.clone(),
            eod_squared_off: self.eod_squared_off_count,
            open_positions_remaining: self.broker.open_positions_count(),
        }
    }

    fn update_last_quotes(&mut self, quotes: &[Quote]) {
        for q in quotes {
            self.last_quotes.insert(q.symbol.clone(), q.clone());
        }
    }

    fn record_buy_fill(&mut self, sig: &Signal, fill: &Fill, snapshot_context: &str) {
        let prev = self.plans.get(&fill.symbol).cloned();
        let entry_price = self
            .broker
            .avg_entry_price(&fill.symbol)
            .unwrap_or(fill.price);
        let sig_trailing_distance = (sig.entry_price - sig.trailing_stop_loss_price).max(0.0);
        let (trailing_stop_loss_price, trailing_distance) = match prev.as_ref() {
            Some(p)
                if !sig.trailing_stop_loss_price.is_finite()
                    || p.trailing_stop_loss_price >= sig.trailing_stop_loss_price =>
            {
                (p.trailing_stop_loss_price, p.trailing_distance)
            }
            _ => (sig.trailing_stop_loss_price, sig_trailing_distance),
        };
        let stop_loss_price = match prev.as_ref() {
            Some(p) if !sig.stop_loss_price.is_finite() => p.stop_loss_price,
            Some(p) => p.stop_loss_price.max(sig.stop_loss_price),
            None => sig.stop_loss_price,
        };
        let highest_price = prev
            .as_ref()
            .map(|p| p.highest_price.max(fill.price))
            .unwrap_or(fill.price);
        let entry_time = prev
            .as_ref()
            .map(|p| p.entry_time)
            .unwrap_or(fill.timestamp);
        self.plans.insert(
            fill.symbol.clone(),
            TrackedPlan {
                entry_price,
                target_price: sig.target_price,
                stop_loss_price,
                trailing_stop_loss_price,
                trailing_distance,
                highest_price,
                entry_time,
                situation_summary: snapshot_context.to_string(),
                decision_rationale: sig.rationale.clone(),
            },
        );
    }

    fn execute_sell_after_signal_recorded(
        &mut self,
        sig: &Signal,
        quote: &Quote,
        snapshot: &MarketSnapshot,
        exit_reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(Fill, TradeOutcome)> {
        let entry_px_before = self.broker.avg_entry_price(&sig.symbol);
        let pre_pnl = self.broker.realized_pnl_paise();
        let fill = self.broker.execute_at(
            sig,
            quote,
            snapshot,
            self.broker.open_positions_count(),
            self.daily_loss_paise,
            self.settings.analysis.stale_after_seconds,
            now,
        )?;
        self.journal.record_fill(&fill)?;

        let delta_pnl_paise = self.broker.realized_pnl_paise() - pre_pnl;
        if delta_pnl_paise < 0 {
            self.daily_loss_paise = self
                .daily_loss_paise
                .saturating_add(delta_pnl_paise.abs());
        }
        let plan = if self.broker.open_position_qty(&fill.symbol) == 0 {
            self.plans.remove(&fill.symbol)
        } else {
            self.plans.get(&fill.symbol).cloned()
        };
        let entry_px = plan
            .as_ref()
            .map(|p| p.entry_price)
            .or(entry_px_before)
            .unwrap_or(fill.price);
        let pnl_rupees = delta_pnl_paise as f64 / 100.0;
        let return_bps = if entry_px > 0.0 {
            (((fill.price - entry_px) / entry_px) * 10_000.0).round() as i32
        } else {
            0
        };
        let entry_time = plan.as_ref().map(|p| p.entry_time).unwrap_or(fill.timestamp);
        self.closed_trades.push(TradeSummary {
            trade_id: fill.signal_id.clone(),
            symbol: fill.symbol.clone(),
            quantity: fill.quantity,
            entry_price: entry_px,
            exit_price: fill.price,
            fees_rupees: fill.fees,
            realized_pnl_paise: delta_pnl_paise,
            realized_pnl_rupees: pnl_rupees,
            return_bps,
            exit_reason: exit_reason.to_string(),
            entry_time,
            exit_time: fill.timestamp,
        });
        let outcome = TradeOutcome {
            trade_id: fill.signal_id.clone(),
            symbol: fill.symbol.clone(),
            side: Side::Buy,
            quantity: fill.quantity,
            entry_price: entry_px,
            exit_price: fill.price,
            pnl_rupees,
            return_bps,
            exit_reason: exit_reason.to_string(),
            entry_time,
            exit_time: fill.timestamp,
            situation_summary: plan
                .as_ref()
                .map(|p| p.situation_summary.clone())
                .unwrap_or_else(|| snapshot.context.clone()),
            regime_tags: vec!["nifty".into(), "paper".into()],
            decision_rationale: plan
                .map(|p| p.decision_rationale)
                .unwrap_or_else(|| sig.rationale.clone()),
        };
        Ok((fill, outcome))
    }

    pub fn monitor_positions_at(
        &mut self,
        snapshot: &MarketSnapshot,
        now: DateTime<Utc>,
    ) -> Result<Vec<Fill>> {
        if LIVE_ORDER_PLACEMENT_IMPLEMENTED {
            bail!("live order placement must remain disabled");
        }
        risk::check_market_snapshot_at(
            snapshot,
            self.settings.analysis.stale_after_seconds,
            now,
        )?;
        self.update_last_quotes(&snapshot.quotes);

        if self.plans.is_empty() {
            return Ok(Vec::new());
        }

        let symbols: Vec<String> = self.plans.keys().cloned().collect();
        let mut fills = Vec::new();

        for symbol in symbols {
            let open_qty = self.broker.open_position_qty(&symbol);
            if open_qty == 0 {
                self.plans.remove(&symbol);
                continue;
            }
            let Some(quote) = snapshot.quotes.iter().find(|q| q.symbol == symbol).cloned() else {
                continue;
            };
            let mark_px = if quote.bid > 0.0 {
                quote.bid
            } else {
                quote.last
            };
            if !mark_px.is_finite() || mark_px <= 0.0 {
                continue;
            }

            let Some(plan_snapshot) = self.plans.get(&symbol).cloned() else {
                continue;
            };

            let exit_reason = if plan_snapshot.stop_loss_price.is_finite()
                && plan_snapshot.stop_loss_price > 0.0
                && mark_px <= plan_snapshot.stop_loss_price
            {
                Some("stop_loss")
            } else if plan_snapshot.trailing_stop_loss_price.is_finite()
                && plan_snapshot.trailing_stop_loss_price > 0.0
                && mark_px <= plan_snapshot.trailing_stop_loss_price
            {
                Some("trailing_stop_loss")
            } else if plan_snapshot.target_price.is_finite()
                && plan_snapshot.target_price > 0.0
                && mark_px >= plan_snapshot.target_price
            {
                Some("target_hit")
            } else {
                None
            };

            if let Some(reason) = exit_reason {
                let sig = Signal {
                    id: format!("exit-{reason}-{symbol}-{}", Uuid::new_v4()),
                    source: "tick_monitor".into(),
                    symbol: symbol.clone(),
                    side: Side::Sell,
                    quantity: open_qty,
                    entry_price: protective_sell_limit(&quote, &self.settings.paper),
                    target_price: plan_snapshot.target_price,
                    stop_loss_price: plan_snapshot.stop_loss_price,
                    trailing_stop_loss_price: plan_snapshot.trailing_stop_loss_price,
                    confidence_bps: 10_000,
                    rationale: format!(
                        "deterministic tick exit ({reason}): mark_px={mark_px:.2}, sl={:.2}, tsl={:.2}, target={:.2}",
                        plan_snapshot.stop_loss_price,
                        plan_snapshot.trailing_stop_loss_price,
                        plan_snapshot.target_price
                    ),
                    snapshot_id: Some(snapshot.id),
                };
                if !self.journal.record_signal(&sig)? {
                    continue;
                }
                match self.execute_sell_after_signal_recorded(&sig, &quote, snapshot, reason, now) {
                    Ok((fill, outcome)) => {
                        self.pending_reflections.push(outcome);
                        fills.push(fill);
                    }
                    Err(err) => {
                        warn!(
                            symbol = %symbol,
                            reason = reason,
                            error = %err,
                            "deterministic tick exit execution failed"
                        );
                    }
                }
            } else if let Some(plan) = self.plans.get_mut(&symbol) {
                let peak = quote.last.max(quote.bid);
                if peak.is_finite() && peak > plan.highest_price {
                    plan.highest_price = peak;
                }
                if plan.trailing_distance.is_finite() && plan.trailing_distance >= 0.0 {
                    let candidate = plan.highest_price - plan.trailing_distance;
                    if candidate.is_finite() && candidate > plan.trailing_stop_loss_price {
                        plan.trailing_stop_loss_price = candidate;
                    }
                }
            }
        }

        Ok(fills)
    }

    pub async fn flush_reflections_at(&mut self, now: DateTime<Utc>) -> Result<usize> {
        let pending = std::mem::take(&mut self.pending_reflections);
        if pending.is_empty() || !self.settings.analysis.reflection_enabled {
            return Ok(0);
        }
        let mut count = 0usize;
        for outcome in pending {
            self.orchestrator
                .reflect_on_trade_at(&outcome, Some(&self.journal), now)
                .await?;
            count += 1;
        }
        Ok(count)
    }

    pub fn square_off_all(
        &mut self,
        snapshot: &MarketSnapshot,
        now: DateTime<Utc>,
    ) -> Result<Vec<Fill>> {
        if LIVE_ORDER_PLACEMENT_IMPLEMENTED {
            bail!("live order placement must remain disabled");
        }

        let open_symbols = self.broker.open_symbols();
        if open_symbols.is_empty() {
            if !snapshot.quotes.is_empty()
                && risk::check_market_snapshot_at(
                    snapshot,
                    self.settings.analysis.stale_after_seconds,
                    now,
                )
                .is_ok()
            {
                self.update_last_quotes(&snapshot.quotes);
            }
            return Ok(Vec::new());
        }

        if !snapshot.quotes.is_empty() {
            risk::check_market_snapshot_at(
                snapshot,
                self.settings.analysis.stale_after_seconds,
                now,
            )?;
            self.update_last_quotes(&snapshot.quotes);
        }

        let mut fills = Vec::new();
        for symbol in open_symbols {
            let open_qty = self.broker.open_position_qty(&symbol);
            if open_qty == 0 {
                continue;
            }

            let live_quote = snapshot.quotes.iter().find(|q| q.symbol == symbol).cloned();
            let (quote, settle_snapshot) = if let Some(q) = live_quote {
                (q, None)
            } else if let Some(mut cached) = self.last_quotes.get(&symbol).cloned() {
                warn!(
                    symbol = %symbol,
                    "symbol missing from EOD snapshot; squaring off using last_quotes fallback"
                );
                cached.timestamp = now;
                let fallback_snap = MarketSnapshot {
                    id: Uuid::new_v4(),
                    created_at: now,
                    quotes: vec![cached.clone()],
                    context: format!("EOD last_quotes fallback for {symbol}"),
                };
                (cached, Some(fallback_snap))
            } else {
                warn!(
                    symbol = %symbol,
                    open_qty = open_qty,
                    "no live or cached quote available for EOD square-off; position remains open"
                );
                continue;
            };

            let active_snap = settle_snapshot.as_ref().unwrap_or(snapshot);
            let plan_opt = self.plans.get(&symbol).cloned();
            let avg_entry = self
                .broker
                .avg_entry_price(&symbol)
                .unwrap_or(quote.bid.max(1.0));
            let sig = Signal {
                id: format!("exit-eod_square_off-{symbol}-{}", Uuid::new_v4()),
                source: "eod_square_off".into(),
                symbol: symbol.clone(),
                side: Side::Sell,
                quantity: open_qty,
                entry_price: protective_sell_limit(&quote, &self.settings.paper),
                target_price: plan_opt
                    .as_ref()
                    .map(|p| p.target_price)
                    .unwrap_or(avg_entry * 1.5),
                stop_loss_price: plan_opt
                    .as_ref()
                    .map(|p| p.stop_loss_price)
                    .unwrap_or((avg_entry * 0.5).max(1e-6)),
                trailing_stop_loss_price: plan_opt
                    .as_ref()
                    .map(|p| p.trailing_stop_loss_price)
                    .unwrap_or((avg_entry * 0.7).max(1e-6)),
                confidence_bps: 10_000,
                rationale: "end-of-day market close square-off at 15:30 IST".into(),
                snapshot_id: Some(active_snap.id),
            };

            if !self.journal.record_signal(&sig)? {
                continue;
            }
            match self.execute_sell_after_signal_recorded(
                &sig,
                &quote,
                active_snap,
                "eod_square_off",
                now,
            ) {
                Ok((fill, outcome)) => {
                    self.eod_squared_off_count += 1;
                    self.pending_reflections.push(outcome);
                    fills.push(fill);
                }
                Err(err) => {
                    warn!(
                        symbol = %symbol,
                        error = %err,
                        "EOD square-off failed for symbol"
                    );
                }
            }
        }

        Ok(fills)
    }

    pub async fn step(&mut self, snapshot: &MarketSnapshot) -> Result<PaperStepResult> {
        self.step_at(snapshot, Utc::now()).await
    }

    pub async fn step_at(
        &mut self,
        snapshot: &MarketSnapshot,
        now: DateTime<Utc>,
    ) -> Result<PaperStepResult> {
        if LIVE_ORDER_PLACEMENT_IMPLEMENTED {
            bail!("live order placement must remain disabled");
        }
        risk::check_market_snapshot_at(
            snapshot,
            self.settings.analysis.stale_after_seconds,
            now,
        )?;
        self.update_last_quotes(&snapshot.quotes);

        let current_pos_ctx = self.active_position_context(snapshot);
        let trace = self
            .orchestrator
            .decide_with_trace(snapshot, current_pos_ctx.as_ref(), Some(&self.journal))
            .await?;

        let mut signal = trace.final_signal.clone();
        if signal.is_none() {
            if let Some(ref strat) = self.strategy {
                signal = strat.evaluate(snapshot).await?;
            }
        }

        let Some(sig) = signal else {
            return Ok(PaperStepResult {
                trace,
                fill: None,
                reflection_recorded: false,
                duplicate_signal_skipped: false,
            });
        };

        let quote = snapshot
            .quotes
            .iter()
            .find(|q| q.symbol == sig.symbol)
            .with_context(|| format!("quote for signal symbol {} not in snapshot", sig.symbol))?;

        let recorded = self.journal.record_signal(&sig)?;
        if !recorded {
            return Ok(PaperStepResult {
                trace,
                fill: None,
                reflection_recorded: false,
                duplicate_signal_skipped: true,
            });
        }

        match sig.side {
            Side::Buy => {
                let fill = self.broker.execute_at(
                    &sig,
                    quote,
                    snapshot,
                    self.broker.open_positions_count(),
                    self.daily_loss_paise,
                    self.settings.analysis.stale_after_seconds,
                    now,
                )?;
                self.journal.record_fill(&fill)?;
                self.record_buy_fill(&sig, &fill, &snapshot.context);
                Ok(PaperStepResult {
                    trace,
                    fill: Some(fill),
                    reflection_recorded: false,
                    duplicate_signal_skipped: false,
                })
            }
            Side::Sell => {
                let (fill, outcome) = self.execute_sell_after_signal_recorded(
                    &sig,
                    quote,
                    snapshot,
                    "agent_exit",
                    now,
                )?;
                let mut reflection_recorded = false;
                if self.settings.analysis.reflection_enabled {
                    self.orchestrator
                        .reflect_on_trade_at(&outcome, Some(&self.journal), now)
                        .await?;
                    reflection_recorded = true;
                }
                Ok(PaperStepResult {
                    trace,
                    fill: Some(fill),
                    reflection_recorded,
                    duplicate_signal_skipped: false,
                })
            }
        }
    }

    pub fn active_position_context(&self, snapshot: &MarketSnapshot) -> Option<PositionContext> {
        let mut preferred: Option<(&String, &TrackedPlan, u32)> = None;
        let mut fallback: Option<(&String, &TrackedPlan, u32)> = None;

        for (symbol, plan) in &self.plans {
            let qty = self.broker.open_position_qty(symbol);
            if qty == 0 {
                continue;
            }
            if snapshot.quotes.iter().any(|q| q.symbol == *symbol) {
                preferred = Some((symbol, plan, qty));
                break;
            }
            if fallback.is_none() {
                fallback = Some((symbol, plan, qty));
            }
        }

        let (symbol, plan, qty) = preferred.or(fallback)?;
        let mark_px = snapshot
            .quotes
            .iter()
            .find(|q| q.symbol == *symbol)
            .map(|q| q.bid)
            .or_else(|| self.last_quotes.get(symbol).map(|q| q.bid))
            .unwrap_or(plan.entry_price);
        Some(PositionContext {
            symbol: symbol.clone(),
            quantity: qty,
            entry_price: plan.entry_price,
            target_price: plan.target_price,
            stop_loss_price: plan.stop_loss_price,
            trailing_stop_loss_price: plan.trailing_stop_loss_price,
            unrealized_pnl_rupees: (mark_px - plan.entry_price) * qty as f64,
            entry_time: Some(plan.entry_time),
        })
    }
}

pub fn protective_sell_limit(quote: &Quote, paper: &Paper) -> f64 {
    let mid = (quote.bid + quote.ask) / 2.0;
    let expected_px =
        mid * (1.0 - (paper.spread_bps as f64 / 2.0 + paper.slippage_bps as f64) / 10_000.0);
    (expected_px * 0.99).max(1e-6)
}

fn time_until_ist_hms(now: DateTime<Utc>, hour: u32, min: u32, sec: u32) -> Duration {
    let ist = to_ist(now);
    let date = ist.date_naive();
    let Some(target_naive) = date.and_hms_opt(hour, min, sec) else {
        return Duration::from_secs(1);
    };
    let Some(target_ist) = ist_offset().from_local_datetime(&target_naive).single() else {
        return Duration::from_secs(1);
    };
    let delta = target_ist.signed_duration_since(ist);
    delta
        .to_std()
        .unwrap_or(Duration::ZERO)
        .max(Duration::from_millis(1))
}

fn is_fatal_feed_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    msg.contains("INDSTOCKS_ACCESS_TOKEN is missing or invalid")
        || msg.contains("INDstocks authentication failed")
        || msg.contains("TokenException")
        || msg.contains("Refresh INDSTOCKS_ACCESS_TOKEN")
        || msg.contains("INDstocks adapter is disabled")
        || msg.contains("verify and configure official INDstocks")
}

async fn apply_analysis_signal(
    runner: &mut PaperRunner,
    signal: Option<Signal>,
    latest_snapshot: &MarketSnapshot,
    now: DateTime<Utc>,
) {
    let Some(mut sig) = signal else {
        return;
    };
    if market_hours_status(now) != MarketHoursStatus::Open {
        info!(
            symbol = %sig.symbol,
            side = ?sig.side,
            "skipping analysis signal because market is no longer open"
        );
        return;
    }
    if sig.side == Side::Sell && runner.broker.open_position_qty(&sig.symbol) == 0 {
        info!(
            symbol = %sig.symbol,
            "skipping stale Sell signal because position was already closed"
        );
        return;
    }
    let Some(quote) = latest_snapshot
        .quotes
        .iter()
        .find(|q| q.symbol == sig.symbol)
        .cloned()
    else {
        warn!(
            symbol = %sig.symbol,
            "skipping analysis signal because quote is absent from latest snapshot"
        );
        return;
    };

    sig.snapshot_id = Some(latest_snapshot.id);
    match runner.journal.record_signal(&sig) {
        Ok(true) => {}
        Ok(false) => return,
        Err(err) => {
            warn!(symbol = %sig.symbol, error = %err, "failed to record analysis signal");
            return;
        }
    }

    match sig.side {
        Side::Buy => {
            match runner.broker.execute_at(
                &sig,
                &quote,
                latest_snapshot,
                runner.broker.open_positions_count(),
                runner.daily_loss_paise,
                runner.settings.analysis.stale_after_seconds,
                now,
            ) {
                Ok(fill) => {
                    if let Err(err) = runner.journal.record_fill(&fill) {
                        warn!(symbol = %fill.symbol, error = %err, "failed to record buy fill");
                    }
                    crate::log_fmt::print_trade_entry(
                        &to_ist(now).format("%d-%b %H:%M:%S IST").to_string(),
                        &fill.symbol,
                        fill.quantity,
                        fill.price,
                        sig.target_price,
                        sig.stop_loss_price,
                        sig.trailing_stop_loss_price,
                        fill.fees,
                        &sig.rationale,
                    );
                    runner.record_buy_fill(&sig, &fill, &latest_snapshot.context);
                }
                Err(err) => {
                    crate::log_fmt::print_trade_rejected(
                        &sig.symbol,
                        &format!("buy signal execution rejected: {err}"),
                    );
                    warn!(symbol = %sig.symbol, error = %err, "buy signal execution rejected");
                }
            }
        }
        Side::Sell => {
            match runner.execute_sell_after_signal_recorded(
                &sig,
                &quote,
                latest_snapshot,
                "agent_exit",
                now,
            ) {
                Ok((_fill, outcome)) => {
                    crate::log_fmt::print_trade_exit(
                        &to_ist(now).format("%d-%b %H:%M:%S IST").to_string(),
                        &outcome.symbol,
                        &outcome.exit_reason,
                        outcome.entry_price,
                        outcome.exit_price,
                        outcome.pnl_rupees,
                        outcome.return_bps,
                    );
                    runner.pending_reflections.push(outcome);
                    if let Err(err) = runner.flush_reflections_at(now).await {
                        warn!(error = %err, "failed to flush post-trade reflection");
                    }
                }
                Err(err) => {
                    warn!(symbol = %sig.symbol, error = %err, "sell signal execution rejected");
                }
            }
        }
    }
}

fn empty_fallback_snapshot(now: DateTime<Utc>, context: &str) -> MarketSnapshot {
    MarketSnapshot {
        id: Uuid::new_v4(),
        created_at: now,
        quotes: Vec::new(),
        context: context.into(),
    }
}

async fn finish_on_shutdown(runner: &mut PaperRunner, now: DateTime<Utc>) -> Result<DailySummary> {
    info!("shutdown requested; squaring off any open paper positions");
    if runner.broker.open_positions_count() > 0 {
        let fallback_snap = empty_fallback_snapshot(now, "Shutdown square-off");
        if let Err(err) = runner.square_off_all(&fallback_snap, now) {
            warn!(error = %err, "shutdown square-off encountered an error");
        }
        if let Err(err) = runner.flush_reflections_at(now).await {
            warn!(error = %err, "shutdown reflection flush failed");
        }
    }
    Ok(runner.daily_summary(now))
}

type AnalysisFuture = Pin<Box<dyn Future<Output = Result<(PipelineTrace, Option<Signal>)>> + Send>>;

pub async fn run_paper_session<F, C, S, Fut, Sh>(
    runner: &mut PaperRunner,
    feed: &F,
    clock: &C,
    mut sleeper: S,
    shutdown: Sh,
) -> Result<DailySummary>
where
    F: MarketDataFeed + ?Sized,
    C: Clock + ?Sized,
    S: FnMut(Duration) -> Fut,
    Fut: Future<Output = ()>,
    Sh: Future<Output = ()>,
{
    if LIVE_ORDER_PLACEMENT_IMPLEMENTED {
        bail!("live order placement must remain disabled");
    }

    tokio::pin!(shutdown);
    let poll_interval = Duration::from_secs(runner.settings.indstocks.poll_interval_seconds.max(1));
    let fetch_timeout =
        Duration::from_secs((runner.settings.analysis.stale_after_seconds.max(15)) as u64 + 5);
    let analysis_interval =
        chrono::Duration::minutes(runner.settings.analysis.bar_minutes.max(1) as i64);

    let mut last_analysis_at: Option<DateTime<Utc>> = None;
    let mut analysis_fut: Option<AnalysisFuture> = None;

    loop {
        let now = clock.now();
        match market_hours_status(now) {
            MarketHoursStatus::PreMarket => {
                let wait = time_until_ist_hms(now, 9, 15, 0).min(Duration::from_secs(300));
                tokio::select! {
                    biased;
                    _ = &mut shutdown => {
                        return finish_on_shutdown(runner, clock.now()).await;
                    }
                    _ = sleeper(wait) => {}
                }
            }
            MarketHoursStatus::Closed => {
                break;
            }
            MarketHoursStatus::Open => {
                let fetched = tokio::select! {
                    biased;
                    _ = &mut shutdown => {
                        return finish_on_shutdown(runner, clock.now()).await;
                    }
                    res = tokio::time::timeout(fetch_timeout, feed.fetch_snapshot(now)) => res,
                };

                let snapshot = match fetched {
                    Ok(Ok(snap)) => snap,
                    Ok(Err(err)) => {
                        if is_fatal_feed_error(&err) {
                            if runner.broker.open_positions_count() > 0 {
                                let fallback_snap =
                                    empty_fallback_snapshot(now, "Fatal feed abort square-off");
                                if let Err(sq_err) = runner.square_off_all(&fallback_snap, now) {
                                    warn!(error = %sq_err, "square-off on fatal feed error failed");
                                }
                                if let Err(ref_err) = runner.flush_reflections_at(now).await {
                                    warn!(error = %ref_err, "reflection flush on fatal feed error failed");
                                }
                            }
                            return Err(err);
                        }
                        warn!(error = %err, "transient market-data fetch error; retrying next poll");
                        let wait = poll_interval.min(time_until_ist_hms(now, 15, 30, 0));
                        tokio::select! {
                            biased;
                            _ = &mut shutdown => {
                                return finish_on_shutdown(runner, clock.now()).await;
                            }
                            _ = sleeper(wait) => {}
                        }
                        continue;
                    }
                    Err(_elapsed) => {
                        warn!(
                            timeout_secs = fetch_timeout.as_secs(),
                            "market-data fetch timed out; retrying next poll"
                        );
                        let wait = poll_interval.min(time_until_ist_hms(now, 15, 30, 0));
                        tokio::select! {
                            biased;
                            _ = &mut shutdown => {
                                return finish_on_shutdown(runner, clock.now()).await;
                            }
                            _ = sleeper(wait) => {}
                        }
                        continue;
                    }
                };

                let exit_fills = match runner.monitor_positions_at(&snapshot, now) {
                    Ok(fills) => fills,
                    Err(err) => {
                        warn!(
                            error = %err,
                            "snapshot failed tick-monitor validation; retrying next poll"
                        );
                        let wait = poll_interval.min(time_until_ist_hms(now, 15, 30, 0));
                        tokio::select! {
                            biased;
                            _ = &mut shutdown => {
                                return finish_on_shutdown(runner, clock.now()).await;
                            }
                            _ = sleeper(wait) => {}
                        }
                        continue;
                    }
                };
                if !exit_fills.is_empty() {
                    if let Err(err) = runner.flush_reflections_at(now).await {
                        warn!(error = %err, "failed to flush tick-exit reflections");
                    }
                }

                if analysis_fut.is_none()
                    && last_analysis_at.is_none_or(|prev| now - prev >= analysis_interval)
                {
                    last_analysis_at = Some(now);
                    let orch = runner.orchestrator.clone();
                    let strat = runner.strategy.clone();
                    let journal = runner.journal.clone();
                    let snap_clone = snapshot.clone();
                    let pos_ctx = runner.active_position_context(&snapshot);
                    analysis_fut = Some(Box::pin(async move {
                        let trace = orch
                            .decide_with_trace(&snap_clone, pos_ctx.as_ref(), Some(&journal))
                            .await?;
                        let mut sig = trace.final_signal.clone();
                        if sig.is_none() {
                            if let Some(ref s) = strat {
                                sig = s.evaluate(&snap_clone).await?;
                            }
                        }
                        Ok((trace, sig))
                    }));
                }

                let wait = poll_interval.min(time_until_ist_hms(now, 15, 30, 0));
                loop {
                    tokio::select! {
                        biased;
                        _ = &mut shutdown => {
                            return finish_on_shutdown(runner, clock.now()).await;
                        }
                        _ = std::future::ready(()) => {}
                    }

                    if let Some(mut fut) = analysis_fut.take() {
                        let polled = std::future::poll_fn(|cx| match fut.as_mut().poll(cx) {
                            std::task::Poll::Ready(res) => std::task::Poll::Ready(Some(res)),
                            std::task::Poll::Pending => std::task::Poll::Ready(None),
                        })
                        .await;
                        if let Some(res) = polled {
                            let apply_now = clock.now();
                            match res {
                                Ok((trace, sig)) => {
                                    crate::log_fmt::print_pipeline_trace(
                                        &to_ist(apply_now).format("%d-%b %H:%M:%S IST").to_string(),
                                        &trace,
                                    );
                                    apply_analysis_signal(runner, sig, &snapshot, apply_now).await;
                                }
                                Err(err) => {
                                    warn!(error = %err, "analysis cycle failed");
                                }
                            }
                            continue;
                        }

                        tokio::select! {
                            biased;
                            _ = &mut shutdown => {
                                return finish_on_shutdown(runner, clock.now()).await;
                            }
                            res = fut.as_mut() => {
                                let apply_now = clock.now();
                                match res {
                                    Ok((_trace, sig)) => {
                                        apply_analysis_signal(runner, sig, &snapshot, apply_now).await;
                                    }
                                    Err(err) => {
                                        warn!(error = %err, "analysis cycle failed");
                                    }
                                }
                            }
                            _ = sleeper(wait) => {
                                analysis_fut = Some(fut);
                                break;
                            }
                        }
                    } else {
                        tokio::select! {
                            biased;
                            _ = &mut shutdown => {
                                return finish_on_shutdown(runner, clock.now()).await;
                            }
                            _ = sleeper(wait) => {}
                        }
                        break;
                    }
                }
            }
        }
    }

    drop(analysis_fut);
    let eod_now = clock.now();
    if runner.broker.open_positions_count() > 0 {
        let closing_snap = match tokio::time::timeout(fetch_timeout, feed.fetch_snapshot(eod_now)).await
        {
            Ok(Ok(snap)) => snap,
            Ok(Err(err)) => {
                warn!(error = %err, "EOD snapshot fetch failed; falling back to cached quotes");
                empty_fallback_snapshot(eod_now, "EOD fallback")
            }
            Err(_) => {
                warn!("EOD snapshot fetch timed out; falling back to cached quotes");
                empty_fallback_snapshot(eod_now, "EOD timeout fallback")
            }
        };
        if let Err(err) = runner.square_off_all(&closing_snap, eod_now) {
            if !closing_snap.quotes.is_empty() {
                warn!(
                    error = %err,
                    "EOD square-off with closing snapshot failed; retrying with cached last_quotes fallback"
                );
                let fallback_snap =
                    empty_fallback_snapshot(eod_now, "EOD stale-snapshot fallback");
                if let Err(fallback_err) = runner.square_off_all(&fallback_snap, eod_now) {
                    warn!(error = %fallback_err, "EOD fallback square-off encountered an error");
                }
            } else {
                warn!(error = %err, "EOD square-off encountered an error");
            }
        }
        if let Err(err) = runner.flush_reflections_at(eod_now).await {
            warn!(error = %err, "EOD reflection flush encountered an error");
        }
    }

    Ok(runner.daily_summary(eod_now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agents::LlmBackend,
        clock::FakeClock,
        config::Settings,
    };
    use async_trait::async_trait;
    use chrono::Duration as ChronoDuration;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    fn test_settings() -> Settings {
        let mut s = Settings::default();
        s.llm.use_offline_deterministic = true;
        s.indstocks.market_data_enabled = true;
        s.indstocks.poll_interval_seconds = 5;
        s.analysis.bar_minutes = 5;
        s.analysis.stale_after_seconds = 15;
        s
    }

    fn make_runner(settings: Settings) -> PaperRunner {
        let journal = Arc::new(Journal::open(":memory:").unwrap());
        let orch = TradingAgentsOrchestrator::deterministic(
            settings.analysis.clone(),
            settings.llm.clone(),
        )
        .with_journal(journal.clone());
        PaperRunner::new(settings, orch, journal).unwrap()
    }

    fn make_snap(now: DateTime<Utc>, quotes: Vec<Quote>, context: &str) -> MarketSnapshot {
        MarketSnapshot {
            id: Uuid::new_v4(),
            created_at: now,
            quotes,
            context: context.into(),
        }
    }

    fn make_quote(symbol: &str, bid: f64, ask: f64, last: f64, now: DateTime<Utc>) -> Quote {
        Quote {
            symbol: symbol.into(),
            bid,
            ask,
            last,
            timestamp: now,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn open_long(
        runner: &mut PaperRunner,
        id: &str,
        symbol: &str,
        qty: u32,
        entry_limit: f64,
        sl: f64,
        tsl: f64,
        target: f64,
        snap: &MarketSnapshot,
        now: DateTime<Utc>,
    ) -> Fill {
        let q = snap
            .quotes
            .iter()
            .find(|q| q.symbol == symbol)
            .unwrap()
            .clone();
        let sig = Signal {
            id: id.into(),
            source: "test".into(),
            symbol: symbol.into(),
            side: Side::Buy,
            quantity: qty,
            entry_price: entry_limit,
            target_price: target,
            stop_loss_price: sl,
            trailing_stop_loss_price: tsl,
            confidence_bps: 8000,
            rationale: "test entry".into(),
            snapshot_id: Some(snap.id),
        };
        assert!(runner.journal.record_signal(&sig).unwrap());
        let fill = runner
            .broker
            .execute_at(
                &sig,
                &q,
                snap,
                runner.broker.open_positions_count(),
                runner.daily_loss_paise(),
                runner.settings.analysis.stale_after_seconds,
                now,
            )
            .unwrap();
        runner.journal.record_fill(&fill).unwrap();
        runner.record_buy_fill(&sig, &fill, &snap.context);
        runner.update_last_quotes(&snap.quotes);
        fill
    }

    #[tokio::test]
    async fn tick_monitor_triggers_stop_loss_with_tight_spread_and_updates_daily_loss() {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();
        let mut runner = make_runner(test_settings());
        let sym = "NIFTY-25000-CE";

        let s0 = make_snap(t0, vec![make_quote(sym, 100.0, 100.0, 100.0, t0)], "open");
        open_long(&mut runner, "b-sl", sym, 5, 101.0, 85.0, 92.0, 140.0, &s0, t0);
        assert_eq!(runner.broker.open_position_qty(sym), 5);

        // Tick at bid == ask == 85.0 (hits stop_loss_price even though TSL=92 also breached -> SL priority)
        let t1 = t0 + ChronoDuration::seconds(5);
        let s1 = make_snap(t1, vec![make_quote(sym, 85.0, 85.0, 85.0, t1)], "drop");
        let fills = runner.monitor_positions_at(&s1, t1).unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].side, Side::Sell);
        assert_eq!(fills[0].quantity, 5);
        assert_eq!(runner.broker.open_position_qty(sym), 0);
        assert!(runner.daily_loss_paise() > 0);
        assert_eq!(runner.closed_trades().len(), 1);
        assert_eq!(runner.closed_trades()[0].exit_reason, "stop_loss");

        let flushed = runner.flush_reflections_at(t1).await.unwrap();
        assert_eq!(flushed, 1);
        assert_eq!(runner.journal.reflection_count().unwrap(), 1);
    }

    #[test]
    fn tick_monitor_ratchets_tsl_upward_never_downward_and_exits_on_pullback() {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();
        let mut runner = make_runner(test_settings());
        let sym = "NIFTY-25000-CE";

        // entry=100.5, sl=80, tsl=90.5 -> trailing_distance = 10.0, target=150
        let s0 = make_snap(t0, vec![make_quote(sym, 100.0, 100.0, 100.0, t0)], "entry");
        open_long(&mut runner, "b-tsl", sym, 4, 100.5, 80.0, 90.5, 150.0, &s0, t0);

        // Tick 1: rally to last=120 -> no exit, highest=120, TSL ratchets to 110
        let t1 = t0 + ChronoDuration::seconds(5);
        let s1 = make_snap(t1, vec![make_quote(sym, 119.0, 121.0, 120.0, t1)], "rally");
        assert!(runner.monitor_positions_at(&s1, t1).unwrap().is_empty());
        let ctx1 = runner.active_position_context(&s1).unwrap();
        assert!((ctx1.trailing_stop_loss_price - 110.0).abs() < 1e-6);

        // Tick 2: minor dip to bid=113, last=113 -> stays open, TSL must NOT move down!
        let t2 = t1 + ChronoDuration::seconds(5);
        let s2 = make_snap(t2, vec![make_quote(sym, 113.0, 114.0, 113.0, t2)], "dip");
        assert!(runner.monitor_positions_at(&s2, t2).unwrap().is_empty());
        let ctx2 = runner.active_position_context(&s2).unwrap();
        assert!((ctx2.trailing_stop_loss_price - 110.0).abs() < 1e-6);

        // Tick 3: pullback to bid=110.0 -> breaches ratcheted TSL=110.0 -> profitable exit!
        let t3 = t2 + ChronoDuration::seconds(5);
        let s3 = make_snap(t3, vec![make_quote(sym, 110.0, 111.0, 110.0, t3)], "tsl");
        let fills = runner.monitor_positions_at(&s3, t3).unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!(runner.broker.open_position_qty(sym), 0);
        assert!(runner.broker.realized_pnl_paise() > 0);
        assert_eq!(runner.closed_trades().len(), 1);
        assert_eq!(runner.closed_trades()[0].exit_reason, "trailing_stop_loss");
    }

    #[test]
    fn tick_monitor_triggers_target_ignores_zero_thresholds_and_retains_partial_plan() {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();
        let mut settings = test_settings();
        settings.paper.partial_fill_fraction = 0.5;
        let mut runner = make_runner(settings);
        let sym = "NIFTY-25000-CE";

        // Buy request qty=8 -> partial fill=4
        let s0 = make_snap(t0, vec![make_quote(sym, 100.0, 100.0, 100.0, t0)], "entry");
        let fill = open_long(&mut runner, "b-tgt", sym, 8, 101.0, 80.0, 90.0, 130.0, &s0, t0);
        assert_eq!(fill.quantity, 4);

        // Also test zero/unset thresholds do not falsely trigger when bid=10.0
        runner.plans.insert(
            "ZERO-TEST".into(),
            TrackedPlan {
                entry_price: 50.0,
                target_price: 0.0,
                stop_loss_price: 0.0,
                trailing_stop_loss_price: 0.0,
                trailing_distance: 0.0,
                highest_price: 50.0,
                entry_time: t0,
                situation_summary: String::new(),
                decision_rationale: String::new(),
            },
        );
        // Zero qty plan in active_position_context is skipped in favor of open sym!
        let s1 = make_snap(
            t0 + ChronoDuration::seconds(5),
            vec![
                make_quote("ZERO-TEST", 10.0, 10.0, 10.0, t0 + ChronoDuration::seconds(5)),
                make_quote(sym, 130.0, 130.0, 130.0, t0 + ChronoDuration::seconds(5)),
            ],
            "target",
        );
        let ctx = runner.active_position_context(&s1).unwrap();
        assert_eq!(ctx.symbol, sym);
        assert_eq!(ctx.quantity, 4);

        // Target hit with partial_fill_fraction=0.5 sells 2 of 4 -> plan must remain!
        let fills = runner
            .monitor_positions_at(&s1, t0 + ChronoDuration::seconds(5))
            .unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].quantity, 2);
        assert_eq!(runner.broker.open_position_qty(sym), 2);
        assert!(runner.active_position_context(&s1).is_some());
        assert_eq!(runner.closed_trades()[0].exit_reason, "target_hit");
    }

    #[test]
    fn active_position_context_prefers_symbol_present_in_snapshot() {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();
        let mut runner = make_runner(test_settings());
        let s0 = make_snap(
            t0,
            vec![
                make_quote("SYM-A", 100.0, 101.0, 100.5, t0),
                make_quote("SYM-B", 120.0, 121.0, 120.5, t0),
            ],
            "two",
        );
        assert!(runner.active_position_context(&s0).is_none());
        open_long(&mut runner, "ba", "SYM-A", 2, 102.0, 80.0, 90.0, 140.0, &s0, t0);
        open_long(&mut runner, "bb", "SYM-B", 3, 122.0, 95.0, 110.0, 160.0, &s0, t0);

        let only_b = make_snap(t0, vec![make_quote("SYM-B", 125.0, 126.0, 125.5, t0)], "only_b");
        let ctx = runner.active_position_context(&only_b).unwrap();
        assert_eq!(ctx.symbol, "SYM-B");
        assert_eq!(ctx.quantity, 3);
    }

    #[test]
    fn square_off_all_handles_live_quotes_last_quotes_fallback_and_unquotable_positions() {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 27, 9, 55, 0).unwrap();
        let mut runner = make_runner(test_settings());

        let s0 = make_snap(
            t0,
            vec![
                make_quote("SYM-LIVE", 100.0, 101.0, 100.5, t0),
                make_quote("SYM-CACHED", 110.0, 111.0, 110.5, t0),
                make_quote("SYM-ORPHAN", 90.0, 91.0, 90.5, t0),
            ],
            "open",
        );
        open_long(&mut runner, "b1", "SYM-LIVE", 2, 102.0, 80.0, 90.0, 150.0, &s0, t0);
        open_long(&mut runner, "b2", "SYM-CACHED", 2, 112.0, 85.0, 100.0, 150.0, &s0, t0);
        open_long(&mut runner, "b3", "SYM-ORPHAN", 2, 92.0, 70.0, 80.0, 150.0, &s0, t0);
        // Simulate missing cached quote for SYM-ORPHAN
        runner.last_quotes.remove("SYM-ORPHAN");

        let t_eod = Utc.with_ymd_and_hms(2026, 11, 27, 10, 0, 0).unwrap(); // 15:30 IST
        let eod_snap = make_snap(
            t_eod,
            vec![make_quote("SYM-LIVE", 115.0, 115.0, 115.0, t_eod)],
            "eod",
        );
        let fills = runner.square_off_all(&eod_snap, t_eod).unwrap();
        assert_eq!(fills.len(), 2);
        assert_eq!(runner.broker.open_position_qty("SYM-LIVE"), 0);
        assert_eq!(runner.broker.open_position_qty("SYM-CACHED"), 0);
        assert_eq!(runner.broker.open_position_qty("SYM-ORPHAN"), 2);

        let summary = runner.daily_summary(t_eod);
        assert_eq!(summary.date_ist, "2026-11-27");
        assert_eq!(summary.eod_squared_off, 2);
        assert_eq!(summary.open_positions_remaining, 1);
        assert_eq!(summary.total_trades, 2);
        let formatted = summary.format_summary();
        assert!(formatted.contains("2026-11-27 IST"));
        assert!(formatted.contains("SYM-LIVE"));
        assert!(formatted.contains("SYM-CACHED"));
        assert!(formatted.contains("eod_square_off"));
    }

    struct ScriptedFeed {
        calls: AtomicUsize,
        fail_transient_on_call: Option<usize>,
        fatal_on_call: Option<usize>,
    }

    impl ScriptedFeed {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail_transient_on_call: None,
                fatal_on_call: None,
            }
        }
    }

    #[async_trait]
    impl MarketDataFeed for ScriptedFeed {
        async fn fetch_snapshot(&self, now: DateTime<Utc>) -> Result<MarketSnapshot> {
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fatal_on_call == Some(idx) {
                bail!("INDstocks authentication failed (HTTP 401): invalid token");
            }
            if self.fail_transient_on_call == Some(idx) {
                bail!("transient 503 upstream timeout");
            }
            let ist = to_ist(now);
            let min_since_open = (ist.hour() * 60 + ist.minute()) as i64 - (9 * 60 + 15);
            // Script prices across the day:
            // - First 60 mins: around 120 -> 140 (enough to trigger TSL ratchet and exit when dipping)
            // - Later: steady 120 so second analysis opens a position held until 15:30 IST EOD square-off
            let (ce_bid, ce_ask, ce_last) = if min_since_open < 30 {
                (120.0, 121.0, 120.5)
            } else if min_since_open < 60 {
                // Exceeds DeterministicLlmBackend target ((121 * 1.05) * 1.25 = 158.8125) -> deterministic tick target_hit!
                (160.0, 161.0, 160.5)
            } else {
                (120.0, 121.0, 120.5)
            };
            Ok(MarketSnapshot {
                id: Uuid::new_v4(),
                created_at: now,
                quotes: vec![Quote {
                    symbol: "NIFTY-2026-11-27-25350-CE".into(),
                    bid: ce_bid,
                    ask: ce_ask,
                    last: ce_last,
                    timestamp: now,
                }],
                context: "HISTORICAL NIFTY BAR; spot O/H/L/C=25300/25360/25290/25350; FLAT".into(),
            })
        }
    }

    use chrono::Timelike;

    #[tokio::test]
    async fn run_paper_session_runs_premarket_to_eod_with_tick_exit_and_eod_square_off() {
        let mut settings = test_settings();
        settings.indstocks.poll_interval_seconds = 900; // 15m ticks for crisp full-day test
        settings.analysis.bar_minutes = 120; // 2h analysis cadence
        let mut runner = make_runner(settings);

        // Start in PreMarket: 09:10:00 IST = 03:40:00 UTC
        let start = Utc.with_ymd_and_hms(2026, 11, 27, 3, 40, 0).unwrap();
        let clock = FakeClock::new(start);
        let mut feed = ScriptedFeed::new();
        feed.fail_transient_on_call = Some(2); // Inject a transient fetch error mid-session

        let sleeper = |d: Duration| {
            let clk = clock.clone();
            async move {
                clk.advance(ChronoDuration::from_std(d).unwrap());
            }
        };

        let summary = run_paper_session(
            &mut runner,
            &feed,
            &clock,
            sleeper,
            std::future::pending::<()>(),
        )
        .await
        .unwrap();

        assert_eq!(summary.date_ist, "2026-11-27");
        assert_eq!(summary.open_positions_remaining, 0);
        // Trade 1 exited via tick target_hit, Trade 2 squared off at 15:30 IST EOD!
        assert!(summary.closed_trades.len() >= 2, "summary={summary:#?}");
        assert!(summary
            .closed_trades
            .iter()
            .any(|t| t.exit_reason == "target_hit"));
        assert!(summary
            .closed_trades
            .iter()
            .any(|t| t.exit_reason == "eod_square_off"));
        assert_eq!(summary.eod_squared_off, 1);
        assert!(summary.realized_pnl_paise != 0);
        assert!(
            (summary.ending_cash_paise - (summary.starting_cash_paise + summary.realized_pnl_paise))
                .abs()
                <= summary.closed_trades.len() as i64
        );
        let text = summary.format_summary();
        assert!(text.contains("Paper Trading Daily Summary"));
        assert!(text.contains("eod_square_off"));
    }

    #[tokio::test]
    async fn run_paper_session_aborts_on_fatal_auth_error_after_squaring_off() {
        let mut settings = test_settings();
        settings.indstocks.poll_interval_seconds = 60;
        let mut runner = make_runner(settings);

        // Start at 09:15:00 IST = 03:45:00 UTC
        let start = Utc.with_ymd_and_hms(2026, 11, 27, 3, 45, 0).unwrap();
        let clock = FakeClock::new(start);
        let mut feed = ScriptedFeed::new();
        feed.fatal_on_call = Some(1); // Tick 0 opens position, Tick 1 fails with HTTP 401

        let sleeper = |d: Duration| {
            let clk = clock.clone();
            async move {
                clk.advance(ChronoDuration::from_std(d).unwrap());
            }
        };

        let err = run_paper_session(
            &mut runner,
            &feed,
            &clock,
            sleeper,
            std::future::pending::<()>(),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("authentication failed"));
        // Position opened on tick 0 must have been squared off on fatal abort!
        assert_eq!(runner.broker.open_positions_count(), 0);
        assert_eq!(runner.closed_trades().len(), 1);
        assert_eq!(runner.closed_trades()[0].exit_reason, "eod_square_off");
    }

    #[tokio::test]
    async fn run_paper_session_graceful_shutdown_squares_off_open_positions() {
        let mut settings = test_settings();
        settings.indstocks.poll_interval_seconds = 60;
        let mut runner = make_runner(settings);

        let start = Utc.with_ymd_and_hms(2026, 11, 27, 3, 45, 0).unwrap();
        let clock = FakeClock::new(start);
        let feed = ScriptedFeed::new();
        let notify = Arc::new(Notify::new());
        let notify_sleeper = notify.clone();
        let tick_count = Arc::new(AtomicUsize::new(0));
        let tick_count_clone = tick_count.clone();

        let clock_for_sleep = clock.clone();
        let sleeper = move |d: Duration| {
            let clk = clock_for_sleep.clone();
            let n = notify_sleeper.clone();
            let tc = tick_count_clone.clone();
            async move {
                clk.advance(ChronoDuration::from_std(d).unwrap());
                if tc.fetch_add(1, Ordering::SeqCst) == 0 {
                    // Trigger shutdown right after first tick opens a position
                    n.notify_one();
                }
            }
        };

        let summary = run_paper_session(
            &mut runner,
            &feed,
            &clock,
            sleeper,
            notify.notified(),
        )
        .await
        .unwrap();

        assert_eq!(summary.open_positions_remaining, 0);
        assert_eq!(summary.eod_squared_off, 1);
        assert_eq!(summary.closed_trades.len(), 1);
    }

    struct SlowSellBackend;

    #[async_trait]
    impl LlmBackend for SlowSellBackend {
        async fn complete_json(
            &self,
            _model: &str,
            system: &str,
            _prompt: &str,
            _max_tokens: u32,
        ) -> Result<serde_json::Value> {
            // Simulate slow stage during analysis while ticks continue
            tokio::time::sleep(Duration::from_millis(15)).await;
            if system.contains("[STAGE:TRADER]") {
                Ok(serde_json::json!({
                    "action": "SELL",
                    "rationale": "slow exit",
                    "confidence_bps": 9000,
                    "signal": {
                        "id": "slow-sell-1",
                        "source": "trader",
                        "symbol": "NIFTY-SLOW-CE",
                        "side": "sell",
                        "quantity": 1,
                        "entry_price": 90.0,
                        "target_price": 150.0,
                        "stop_loss_price": 80.0,
                        "trailing_stop_loss_price": 90.0,
                        "confidence_bps": 9000,
                        "rationale": "slow exit",
                        "snapshot_id": null
                    }
                }))
            } else {
                crate::agents::DeterministicLlmBackend
                    .complete_json(_model, system, _prompt, _max_tokens)
                    .await
            }
        }
    }

    #[tokio::test]
    async fn stale_slow_sell_signal_skipped_after_tick_monitor_already_closed() {
        let mut settings = test_settings();
        settings.indstocks.poll_interval_seconds = 5;
        settings.analysis.bar_minutes = 5;
        let journal = Arc::new(Journal::open(":memory:").unwrap());
        let orch = TradingAgentsOrchestrator::new(
            settings.analysis.clone(),
            settings.llm.clone(),
            Arc::new(SlowSellBackend),
        )
        .with_journal(journal.clone());
        let mut runner = PaperRunner::new(settings, orch, journal).unwrap();

        let start = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();
        let clock = FakeClock::new(start);
        let sym = "NIFTY-SLOW-CE";
        let s0 = make_snap(start, vec![make_quote(sym, 100.0, 100.0, 100.0, start)], "open");
        open_long(&mut runner, "b-slow", sym, 1, 101.0, 80.0, 90.0, 140.0, &s0, start);

        struct SlowFeed;
        #[async_trait]
        impl MarketDataFeed for SlowFeed {
            async fn fetch_snapshot(&self, now: DateTime<Utc>) -> Result<MarketSnapshot> {
                // On tick 1 (start + 5s), price hits SL=80 -> tick monitor closes before SlowSellBackend finishes!
                let bid = if now > Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap() {
                    80.0
                } else {
                    100.0
                };
                Ok(MarketSnapshot {
                    id: Uuid::new_v4(),
                    created_at: now,
                    quotes: vec![Quote {
                        symbol: "NIFTY-SLOW-CE".into(),
                        bid,
                        ask: bid,
                        last: bid,
                        timestamp: now,
                    }],
                    context: "slow".into(),
                })
            }
        }

        let ticks = Arc::new(AtomicUsize::new(0));
        let ticks_c = ticks.clone();
        let notify = Arc::new(Notify::new());
        let notify_c = notify.clone();
        let clock_for_sleep = clock.clone();
        let sleeper = move |d: Duration| {
            let clk = clock_for_sleep.clone();
            let tc = ticks_c.clone();
            let n = notify_c.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(25)).await;
                clk.advance(ChronoDuration::from_std(d).unwrap());
                if tc.fetch_add(1, Ordering::SeqCst) >= 2 {
                    n.notify_one();
                }
            }
        };

        let summary = run_paper_session(&mut runner, &SlowFeed, &clock, sleeper, notify.notified())
            .await
            .unwrap();
        assert_eq!(summary.closed_trades.len(), 1);
        assert_eq!(summary.closed_trades[0].exit_reason, "stop_loss");
        assert_eq!(summary.open_positions_remaining, 0);
    }

    #[tokio::test]
    async fn run_paper_session_falls_back_to_last_quotes_when_eod_snapshot_is_stale() {
        let mut settings = test_settings();
        settings.indstocks.poll_interval_seconds = 60;
        settings.analysis.bar_minutes = 5;
        settings.analysis.stale_after_seconds = 15;
        let mut runner = make_runner(settings);

        // Start at 15:28:00 IST (09:58:00 UTC) so position opens during Open and hits EOD at 15:30 IST.
        let start = Utc.with_ymd_and_hms(2026, 11, 27, 9, 58, 0).unwrap();
        let clock = FakeClock::new(start);

        struct StaleEodFeed;
        #[async_trait]
        impl MarketDataFeed for StaleEodFeed {
            async fn fetch_snapshot(&self, now: DateTime<Utc>) -> Result<MarketSnapshot> {
                if market_hours_status(now) == MarketHoursStatus::Closed {
                    // Non-empty snapshot stamped 45s earlier (> stale_after_seconds = 15).
                    let stale_ts = now - ChronoDuration::seconds(45);
                    return Ok(MarketSnapshot {
                        id: Uuid::new_v4(),
                        created_at: stale_ts,
                        quotes: vec![Quote {
                            symbol: "NIFTY-2026-11-27-25350-CE".into(),
                            bid: 122.0,
                            ask: 123.0,
                            last: 122.5,
                            timestamp: stale_ts,
                        }],
                        context: "stale EOD snapshot".into(),
                    });
                }
                Ok(MarketSnapshot {
                    id: Uuid::new_v4(),
                    created_at: now,
                    quotes: vec![Quote {
                        symbol: "NIFTY-2026-11-27-25350-CE".into(),
                        bid: 120.0,
                        ask: 121.0,
                        last: 120.5,
                        timestamp: now,
                    }],
                    context: "HISTORICAL NIFTY BAR; spot O/H/L/C=25300/25360/25290/25350; FLAT"
                        .into(),
                })
            }
        }

        let sleeper = |d: Duration| {
            let clk = clock.clone();
            async move {
                clk.advance(ChronoDuration::from_std(d).unwrap());
            }
        };

        let summary = run_paper_session(
            &mut runner,
            &StaleEodFeed,
            &clock,
            sleeper,
            std::future::pending::<()>(),
        )
        .await
        .unwrap();

        assert_eq!(summary.open_positions_remaining, 0);
        assert_eq!(summary.eod_squared_off, 1);
        assert_eq!(summary.total_trades, 1);
        assert_eq!(summary.closed_trades.len(), 1);
        assert_eq!(summary.closed_trades[0].exit_reason, "eod_square_off");
        assert!(
            (summary.ending_cash_paise - (summary.starting_cash_paise + summary.realized_pnl_paise))
                .abs()
                <= 1
        );
        let text = summary.format_summary();
        assert!(text.contains("eod_square_off"));
        assert!(text.contains("Open Remaining: 0"));
    }

    #[tokio::test]
    async fn scale_in_buy_preserves_ratcheted_trailing_stop_loss_and_distance() {
        struct ScaleInBuyBackend;

        #[async_trait]
        impl LlmBackend for ScaleInBuyBackend {
            async fn complete_json(
                &self,
                model: &str,
                system: &str,
                prompt: &str,
                max_tokens: u32,
            ) -> Result<serde_json::Value> {
                if system.contains("[STAGE:TRADER]") {
                    Ok(serde_json::json!({
                        "action": "BUY",
                        "rationale": "scale-in momentum entry",
                        "confidence_bps": 8000,
                        "signal": {
                            "id": "scale-in-buy-1",
                            "source": "trader",
                            "symbol": "SYM-SCALE",
                            "side": "buy",
                            "quantity": 1,
                            "entry_price": 142.0,
                            "target_price": 180.0,
                            "stop_loss_price": 75.0,
                            "trailing_stop_loss_price": 110.0,
                            "confidence_bps": 8000,
                            "rationale": "scale-in buy",
                            "snapshot_id": null
                        }
                    }))
                } else {
                    crate::agents::DeterministicLlmBackend
                        .complete_json(model, system, prompt, max_tokens)
                        .await
                }
            }
        }

        let settings = test_settings();
        let journal = Arc::new(Journal::open(":memory:").unwrap());
        let orch = TradingAgentsOrchestrator::new(
            settings.analysis.clone(),
            settings.llm.clone(),
            Arc::new(ScaleInBuyBackend),
        )
        .with_journal(journal.clone());
        let mut runner = PaperRunner::new(settings, orch, journal).unwrap();

        let t0 = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();
        let sym = "SYM-SCALE";
        // entry_limit=100.5, sl=80.0, tsl=90.5 -> trailing_distance = 10.0
        let s0 = make_snap(t0, vec![make_quote(sym, 100.0, 100.0, 100.0, t0)], "open");
        open_long(&mut runner, "b-scale-0", sym, 1, 100.5, 80.0, 90.5, 180.0, &s0, t0);

        // Rally to 140.0 ratchets TSL to 140.0 - 10.0 = 130.0
        let t1 = t0 + ChronoDuration::seconds(5);
        let s1 = make_snap(t1, vec![make_quote(sym, 139.0, 141.0, 140.0, t1)], "rally");
        assert!(runner.monitor_positions_at(&s1, t1).unwrap().is_empty());
        let ctx1 = runner.active_position_context(&s1).unwrap();
        assert!((ctx1.trailing_stop_loss_price - 130.0).abs() < 1e-6);

        // Scale-in Buy with lower signal TSL=110.0 and SL=75.0 via step_at
        let t2 = t1 + ChronoDuration::seconds(5);
        let s2 = make_snap(t2, vec![make_quote(sym, 139.0, 140.0, 140.0, t2)], "scale_in");
        let step_res = runner.step_at(&s2, t2).await.unwrap();
        assert!(step_res.fill.is_some());
        assert_eq!(runner.broker.open_position_qty(sym), 2);

        let ctx2 = runner.active_position_context(&s2).unwrap();
        assert!(
            ctx2.trailing_stop_loss_price >= 130.0,
            "ratcheted TSL regressed: {}",
            ctx2.trailing_stop_loss_price
        );
        assert!((ctx2.trailing_stop_loss_price - 130.0).abs() < 1e-6);
        assert!((ctx2.stop_loss_price - 80.0).abs() < 1e-6);

        // Subsequent rally to 145.0 continues ratcheting using preserved trailing_distance=10.0 -> 135.0
        let t3 = t2 + ChronoDuration::seconds(5);
        let s3 = make_snap(t3, vec![make_quote(sym, 144.0, 146.0, 145.0, t3)], "rally2");
        assert!(runner.monitor_positions_at(&s3, t3).unwrap().is_empty());
        let ctx3 = runner.active_position_context(&s3).unwrap();
        assert!((ctx3.trailing_stop_loss_price - 135.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn run_paper_session_retries_on_stale_snapshot_in_monitor_positions() {
        let mut settings = test_settings();
        settings.indstocks.poll_interval_seconds = 60;
        settings.analysis.bar_minutes = 5;
        settings.analysis.stale_after_seconds = 15;
        let mut runner = make_runner(settings);

        let start = Utc.with_ymd_and_hms(2026, 11, 27, 9, 57, 0).unwrap(); // 15:27 IST
        let clock = FakeClock::new(start);

        struct StaleMidSessionFeed {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl MarketDataFeed for StaleMidSessionFeed {
            async fn fetch_snapshot(&self, now: DateTime<Utc>) -> Result<MarketSnapshot> {
                let idx = self.calls.fetch_add(1, Ordering::SeqCst);
                let ts = if idx == 1 {
                    // Second poll returns a stale non-empty snapshot -> monitor_positions_at errs,
                    // run_paper_session must warn + retry rather than aborting with open position!
                    now - ChronoDuration::seconds(45)
                } else {
                    now
                };
                Ok(MarketSnapshot {
                    id: Uuid::new_v4(),
                    created_at: ts,
                    quotes: vec![Quote {
                        symbol: "NIFTY-2026-11-27-25350-CE".into(),
                        bid: 120.0,
                        ask: 121.0,
                        last: 120.5,
                        timestamp: ts,
                    }],
                    context: "HISTORICAL NIFTY BAR; spot O/H/L/C=25300/25360/25290/25350; FLAT"
                        .into(),
                })
            }
        }

        let feed = StaleMidSessionFeed {
            calls: AtomicUsize::new(0),
        };
        let sleeper = |d: Duration| {
            let clk = clock.clone();
            async move {
                clk.advance(ChronoDuration::from_std(d).unwrap());
            }
        };

        let summary = run_paper_session(
            &mut runner,
            &feed,
            &clock,
            sleeper,
            std::future::pending::<()>(),
        )
        .await
        .unwrap();

        assert_eq!(summary.open_positions_remaining, 0);
        assert_eq!(summary.eod_squared_off, 1);
        assert_eq!(summary.closed_trades.len(), 1);
        assert_eq!(summary.closed_trades[0].exit_reason, "eod_square_off");
    }
}

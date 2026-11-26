use crate::{
    agents::TradingAgentsOrchestrator,
    config::Settings,
    journal::Journal,
    paper_broker::PaperBroker,
    risk,
    strategy::IndependentStrategy,
    types::{Fill, MarketSnapshot, PipelineTrace, PositionContext, Side, TradeOutcome},
    LIVE_ORDER_PLACEMENT_IMPLEMENTED,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug)]
struct TrackedPlan {
    entry_price: f64,
    target_price: f64,
    stop_loss_price: f64,
    trailing_stop_loss_price: f64,
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

pub struct PaperRunner {
    pub settings: Settings,
    pub orchestrator: TradingAgentsOrchestrator,
    pub broker: PaperBroker,
    pub journal: Arc<Journal>,
    pub strategy: Option<Arc<dyn IndependentStrategy>>,
    plans: BTreeMap<String, TrackedPlan>,
    daily_loss_paise: i64,
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
        Ok(Self {
            settings,
            orchestrator,
            broker,
            journal,
            strategy: None,
            plans: BTreeMap::new(),
            daily_loss_paise: 0,
        })
    }

    pub fn with_strategy(mut self, strategy: Arc<dyn IndependentStrategy>) -> Self {
        self.strategy = Some(strategy);
        self
    }

    pub fn daily_loss_paise(&self) -> i64 {
        self.daily_loss_paise
    }

    pub async fn step(&mut self, snapshot: &MarketSnapshot) -> Result<PaperStepResult> {
        if LIVE_ORDER_PLACEMENT_IMPLEMENTED {
            bail!("live order placement must remain disabled");
        }
        risk::check_market_snapshot(snapshot, self.settings.analysis.stale_after_seconds)?;

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

        let pre_pnl = self.broker.realized_pnl_paise();
        let fill = self.broker.execute(
            &sig,
            quote,
            snapshot,
            self.broker.open_positions_count(),
            self.daily_loss_paise,
            self.settings.analysis.stale_after_seconds,
        )?;
        self.journal.record_fill(&fill)?;

        let mut reflection_recorded = false;
        match fill.side {
            Side::Buy => {
                self.plans.insert(
                    fill.symbol.clone(),
                    TrackedPlan {
                        entry_price: fill.price,
                        target_price: sig.target_price,
                        stop_loss_price: sig.stop_loss_price,
                        trailing_stop_loss_price: sig.trailing_stop_loss_price,
                        entry_time: fill.timestamp,
                        situation_summary: snapshot.context.clone(),
                        decision_rationale: sig.rationale.clone(),
                    },
                );
            }
            Side::Sell => {
                let delta_pnl_paise = self.broker.realized_pnl_paise() - pre_pnl;
                if delta_pnl_paise < 0 {
                    self.daily_loss_paise = self
                        .daily_loss_paise
                        .saturating_add(delta_pnl_paise.abs());
                }
                let plan = self.plans.remove(&fill.symbol);
                let entry_px = plan.as_ref().map(|p| p.entry_price).unwrap_or(fill.price);
                let pnl_rupees = delta_pnl_paise as f64 / 100.0;
                let return_bps = if entry_px > 0.0 {
                    (((fill.price - entry_px) / entry_px) * 10_000.0).round() as i32
                } else {
                    0
                };
                let outcome = TradeOutcome {
                    trade_id: fill.signal_id.clone(),
                    symbol: fill.symbol.clone(),
                    side: Side::Buy,
                    quantity: fill.quantity,
                    entry_price: entry_px,
                    exit_price: fill.price,
                    pnl_rupees,
                    return_bps,
                    exit_reason: "agent_exit".into(),
                    entry_time: plan.as_ref().map(|p| p.entry_time).unwrap_or(fill.timestamp),
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
                if self.settings.analysis.reflection_enabled {
                    self.orchestrator
                        .reflect_on_trade(&outcome, Some(&self.journal))
                        .await?;
                    reflection_recorded = true;
                }
            }
        }

        Ok(PaperStepResult {
            trace,
            fill: Some(fill),
            reflection_recorded,
            duplicate_signal_skipped: false,
        })
    }

    fn active_position_context(&self, snapshot: &MarketSnapshot) -> Option<PositionContext> {
        let (symbol, plan) = self.plans.iter().next()?;
        let qty = self.broker.open_position_qty(symbol);
        if qty == 0 {
            return None;
        }
        let mark_px = snapshot
            .quotes
            .iter()
            .find(|q| q.symbol == *symbol)
            .map(|q| q.bid)
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

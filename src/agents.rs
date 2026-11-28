use crate::{
    backtest::{BacktestPolicy, BacktestPosition},
    config::{Analysis, Llm, Settings},
    journal::Journal,
    risk,
    types::{
        AnalystReport, AnalystRole, DebateTurn, InvestDebateState, MarketSnapshot, PipelineTrace,
        PositionContext, ResearchDecision, RetrievedMemory, RiskAnalystRole, RiskDebateState,
        RiskVerdict, Side, Signal, TradeOutcome, TradeReflection, TraderProposal,
    },
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{Mutex, Semaphore},
    time::Instant,
};

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn complete_json(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<Value>;
}

#[derive(Clone)]
pub struct GeminiLlmBackend {
    client: reqwest::Client,
    config: Llm,
    key: String,
    gate: Arc<Semaphore>,
    last_request: Arc<Mutex<Option<Instant>>>,
    fallback_active: Arc<AtomicBool>,
}

#[derive(Serialize)]
struct GeminiRequest<'a> {
    model: &'a str,
    messages: [GeminiMessage<'a>; 1],
    temperature: f32,
    max_tokens: u32,
    reasoning_effort: &'static str,
    response_format: GeminiFormat,
}

#[derive(Serialize)]
struct GeminiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct GeminiFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Deserialize)]
struct GeminiResponse {
    choices: Vec<GeminiChoice>,
}

#[derive(Deserialize)]
struct GeminiChoice {
    message: GeminiContent,
}

#[derive(Deserialize)]
struct GeminiContent {
    content: String,
}

impl GeminiLlmBackend {
    pub fn new(config: Llm, key: String) -> Self {
        let permits = config.max_concurrent_requests.max(1);
        Self {
            client: reqwest::Client::new(),
            config,
            key,
            gate: Arc::new(Semaphore::new(permits)),
            last_request: Arc::new(Mutex::new(None)),
            fallback_active: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn wait_for_request_slot(&self) -> Result<()> {
        let min_gap =
            Duration::from_secs_f64(60.0 / self.config.requests_per_minute.max(1) as f64);
        let mut last = self.last_request.lock().await;
        if let Some(at) = *last {
            tokio::time::sleep(min_gap.saturating_sub(at.elapsed())).await;
        }
        *last = Some(Instant::now());
        Ok(())
    }
}

#[async_trait]
impl LlmBackend for GeminiLlmBackend {
    async fn complete_json(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<Value> {
        if model.starts_with("SET_") || self.config.fallback_model.starts_with("SET_") {
            bail!("configure Gemini model IDs in settings.toml");
        }
        let _permit = self.gate.acquire().await?;
        let user_prompt = format!(
            "{system}\n\nReturn exactly one valid JSON object and no Markdown.\n\nInput and evidence:\n{prompt}"
        );
        let mut retry = 0u32;
        let response = loop {
            self.wait_for_request_slot().await?;
            let selected_model = if self.fallback_active.load(Ordering::Relaxed) {
                self.config.fallback_model.as_str()
            } else {
                model
            };
            let body = GeminiRequest {
                model: selected_model,
                messages: [GeminiMessage {
                    role: "user",
                    content: &user_prompt,
                }],
                temperature: self.config.temperature,
                max_tokens,
                reasoning_effort: "minimal",
                response_format: GeminiFormat {
                    kind: "json_object",
                },
            };
            let response = self
                .client
                .post(&self.config.base_url)
                .bearer_auth(&self.key)
                .json(&body)
                .send()
                .await?;
            if response.status() != reqwest::StatusCode::SERVICE_UNAVAILABLE {
                break response;
            }
            let _ = response.bytes().await;
            let is_fallback = selected_model == self.config.fallback_model;
            if is_fallback && retry >= 2 {
                bail!("Gemini primary and fallback models are unavailable (HTTP 503)");
            }
            if !is_fallback && retry >= 2 {
                if !self.config.fallback_model.is_empty() && self.config.fallback_model != model {
                    self.fallback_active.store(true, Ordering::Relaxed);
                    retry = 0;
                    continue;
                }
                bail!("Gemini model unavailable and no distinct fallback model is configured");
            }
            tokio::time::sleep(Duration::from_secs(2u64.pow(retry))).await;
            retry += 1;
        };
        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            bail!("Gemini API returned HTTP {status}: {error_body}");
        }
        let response: GeminiResponse = response.json().await.context("decode Gemini response")?;
        let raw_json = response
            .choices
            .first()
            .context("Gemini returned no choices")?
            .message
            .content
            .trim();
        let cleaned = strip_markdown_fences(raw_json);
        serde_json::from_str(cleaned).context("parse structured Gemini JSON response")
    }
}

/// Offline deterministic backend for unit/E2E tests and key-free local paper/backtest runs.
#[derive(Clone, Debug, Default)]
pub struct DeterministicLlmBackend;

#[async_trait]
impl LlmBackend for DeterministicLlmBackend {
    async fn complete_json(
        &self,
        _model: &str,
        system: &str,
        prompt: &str,
        _max_tokens: u32,
    ) -> Result<Value> {
        let payload: Value = serde_json::from_str(prompt).unwrap_or_else(|_| json!({}));

        if system.contains("[STAGE:ANALYST_TECHNICAL]") {
            let snapshot: MarketSnapshot =
                serde_json::from_value(payload["snapshot"].clone()).context("decode snapshot")?;
            let (stance, summary, levels) = evaluate_technical(&snapshot);
            return Ok(serde_json::to_value(AnalystReport {
                role: AnalystRole::Technical,
                stance,
                confidence_bps: 7200,
                summary,
                key_levels: levels,
                evidence: snapshot.context.clone(),
                regime_tags: vec!["nifty".into(), "intraday_5m".into(), "technical".into()],
            })?);
        }

        if system.contains("[STAGE:ANALYST_OPTIONS]") {
            let snapshot: MarketSnapshot =
                serde_json::from_value(payload["snapshot"].clone()).context("decode snapshot")?;
            let best = pick_best_option(&snapshot);
            let (summary, levels) = match best {
                Some(q) => (
                    format!(
                        "Tightest liquid contract {} bid={:.2} ask={:.2} last={:.2}",
                        q.symbol, q.bid, q.ask, q.last
                    ),
                    vec![format!("{:.2}", q.bid), format!("{:.2}", q.ask)],
                ),
                None => ("No liquid option contract available".into(), vec![]),
            };
            return Ok(serde_json::to_value(AnalystReport {
                role: AnalystRole::OptionsVolatility,
                stance: "bullish".into(),
                confidence_bps: 7000,
                summary,
                key_levels: levels,
                evidence: "Option chain spread and premium assessment".into(),
                regime_tags: vec!["options".into(), "volatility".into()],
            })?);
        }

        if system.contains("[STAGE:ANALYST_REGIME]") {
            let snapshot: MarketSnapshot =
                serde_json::from_value(payload["snapshot"].clone()).context("decode snapshot")?;
            let mem_count = payload["memories"]
                .as_array()
                .map(|m| m.len())
                .unwrap_or(0);
            return Ok(serde_json::to_value(AnalystReport {
                role: AnalystRole::RegimeSentiment,
                stance: "neutral_bullish".into(),
                confidence_bps: 6800,
                summary: format!(
                    "Regime balanced across {} quotes with {} recalled memories",
                    snapshot.quotes.len(),
                    mem_count
                ),
                key_levels: vec![],
                evidence: snapshot.context,
                regime_tags: vec!["regime".into(), "sentiment".into()],
            })?);
        }

        if system.contains("[STAGE:RESEARCH_BULL]") {
            let round = payload["round"].as_u64().unwrap_or(1) as usize;
            return Ok(serde_json::to_value(DebateTurn {
                speaker: "bull_researcher".into(),
                round,
                argument: "Bull case: option chain liquidity and momentum support a defined-risk long option entry.".into(),
                confidence_bps: 7400,
            })?);
        }

        if system.contains("[STAGE:RESEARCH_BEAR]") {
            let round = payload["round"].as_u64().unwrap_or(1) as usize;
            return Ok(serde_json::to_value(DebateTurn {
                speaker: "bear_researcher".into(),
                round,
                argument: "Bear case: theta decay and bid-ask slippage require strict stop_loss_price and trailing_stop_loss_price discipline.".into(),
                confidence_bps: 6200,
            })?);
        }

        if system.contains("[STAGE:RESEARCH_MANAGER]") {
            return Ok(serde_json::to_value(ResearchDecision {
                stance: "bullish".into(),
                confidence_bps: 7300,
                thesis: "Synthesized bull momentum with bear risk discipline favors a single-lot defined-risk option trade.".into(),
                investment_plan: "Enter liquid option with strict stop < trail <= entry < target.".into(),
                preferred_right: Some("CE".into()),
                key_risks: vec!["theta_decay".into(), "spread_slippage".into()],
            })?);
        }

        if system.contains("[STAGE:TRADER]") {
            let snapshot: MarketSnapshot =
                serde_json::from_value(payload["snapshot"].clone()).context("decode snapshot")?;
            let position: Option<PositionContext> =
                serde_json::from_value(payload["position"].clone()).ok();

            if let Some(pos) = position {
                if let Some(q) = snapshot.quotes.iter().find(|q| q.symbol == pos.symbol) {
                    let should_exit = q.bid >= pos.target_price
                        || q.bid <= pos.stop_loss_price
                        || q.bid <= pos.trailing_stop_loss_price;
                    if should_exit {
                        let sig = Signal {
                            id: format!("ta-exit-{}-{}", pos.symbol, snapshot.id),
                            source: "trading_agents_trader".into(),
                            symbol: pos.symbol,
                            side: Side::Sell,
                            quantity: pos.quantity.max(1),
                            entry_price: q.bid.max(0.05),
                            target_price: pos.target_price,
                            stop_loss_price: pos.stop_loss_price,
                            trailing_stop_loss_price: pos.trailing_stop_loss_price,
                            confidence_bps: 7600,
                            rationale: "Deterministic exit on target/stop threshold".into(),
                            snapshot_id: Some(snapshot.id),
                        };
                        return Ok(serde_json::to_value(TraderProposal {
                            action: "SELL".into(),
                            rationale: "Closing existing long position".into(),
                            confidence_bps: 7600,
                            signal: Some(sig),
                        })?);
                    }
                }
                return Ok(serde_json::to_value(TraderProposal {
                    action: "HOLD".into(),
                    rationale: "Existing long position remains within target and protective stops"
                        .into(),
                    confidence_bps: 7000,
                    signal: None,
                })?);
            }

            if let Some(q) = pick_best_option(&snapshot) {
                let entry_price = (q.ask * 1.05).max(q.ask + 0.5);
                let target_price = (entry_price * 1.25).max(entry_price + 2.0);
                let stop_loss_price = (entry_price * 0.70).max(0.05);
                let trailing_stop_loss_price = (entry_price * 0.85).max(stop_loss_price + 0.05);
                let sig = Signal {
                    id: format!("ta-buy-{}-{}", q.symbol, snapshot.id),
                    source: "trading_agents_trader".into(),
                    symbol: q.symbol.clone(),
                    side: Side::Buy,
                    quantity: 1,
                    entry_price,
                    target_price,
                    stop_loss_price,
                    trailing_stop_loss_price,
                    confidence_bps: 7400,
                    rationale: format!("Defined-risk long option entry on {}", q.symbol),
                    snapshot_id: Some(snapshot.id),
                };
                return Ok(serde_json::to_value(TraderProposal {
                    action: "BUY".into(),
                    rationale: "Proposing 1-lot defined-risk long option".into(),
                    confidence_bps: 7400,
                    signal: Some(sig),
                })?);
            }

            return Ok(serde_json::to_value(TraderProposal {
                action: "HOLD".into(),
                rationale: "No valid quote in snapshot".into(),
                confidence_bps: 5000,
                signal: None,
            })?);
        }

        if system.contains("[STAGE:RISK_ANALYST]") {
            let role = payload["role"].as_str().unwrap_or("neutral");
            let round = payload["round"].as_u64().unwrap_or(1) as usize;
            return Ok(serde_json::to_value(DebateTurn {
                speaker: format!("{role}_risk_analyst"),
                round,
                argument: format!(
                    "{role} risk review: verified defined-risk levels and position sizing."
                ),
                confidence_bps: 7300,
            })?);
        }

        if system.contains("[STAGE:RISK_MANAGER]") {
            let proposal: TraderProposal =
                serde_json::from_value(payload["trader_proposal"].clone())
                    .context("decode trader_proposal")?;
            let approved = proposal
                .signal
                .as_ref()
                .is_some_and(|s| risk::check_agent_plan(s).is_ok());
            return Ok(serde_json::to_value(RiskVerdict {
                approved: approved || proposal.signal.is_none(),
                decision: if proposal.signal.is_some() {
                    if approved {
                        "APPROVE".into()
                    } else {
                        "VETO".into()
                    }
                } else {
                    "HOLD".into()
                },
                adjusted_confidence_bps: proposal.confidence_bps,
                verdict_rationale: "Risk Manager validated plan invariants and exposure".into(),
                final_signal: if approved { proposal.signal } else { None },
            })?);
        }

        if system.contains("[STAGE:REFLECTOR]") {
            let outcome: TradeOutcome =
                serde_json::from_value(payload["outcome"].clone()).context("decode outcome")?;
            let outcome_label = if outcome.pnl_rupees >= 0.0 {
                "profitable"
            } else {
                "loss-making"
            };
            return Ok(serde_json::to_value(json!({
                "lesson_bull": format!("Bull thesis on {} yielded {} exit ({})", outcome.symbol, outcome_label, outcome.exit_reason),
                "lesson_bear": format!("Bear risk check noted exit_reason={} with return {} bps", outcome.exit_reason, outcome.return_bps),
                "lesson_trader": format!("Entry {:.2} -> Exit {:.2}; calibrate target and trailing stop spacing", outcome.entry_price, outcome.exit_price),
                "lesson_risk": format!("Position sizing and stop discipline produced PnL {:.2} INR", outcome.pnl_rupees),
                "lesson_summary": format!("{} trade on {} ({}): PnL {:.2} INR ({} bps)", outcome_label, outcome.symbol, outcome.exit_reason, outcome.pnl_rupees, outcome.return_bps),
            }))?);
        }

        bail!("unknown deterministic stage prompt: {system}")
    }
}

fn evaluate_technical(snapshot: &MarketSnapshot) -> (String, String, Vec<String>) {
    if let Some(q) = snapshot.quotes.first() {
        let spread = (q.ask - q.bid).max(0.0);
        let stance = if q.last >= (q.bid + q.ask) / 2.0 {
            "bullish"
        } else {
            "neutral"
        };
        (
            stance.into(),
            format!(
                "Primary quote {} last={:.2} spread={:.2}",
                q.symbol, q.last, spread
            ),
            vec![format!("{:.2}", q.bid), format!("{:.2}", q.ask)],
        )
    } else {
        ("neutral".into(), "Empty quote list".into(), vec![])
    }
}

fn pick_best_option(snapshot: &MarketSnapshot) -> Option<&crate::types::Quote> {
    snapshot
        .quotes
        .iter()
        .filter(|q| q.bid > 0.0 && q.ask >= q.bid)
        .min_by(|a, b| {
            let sa = (a.ask - a.bid) / a.ask.max(1.0);
            let sb = (b.ask - b.bid) / b.ask.max(1.0);
            sa.total_cmp(&sb).then_with(|| a.ask.total_cmp(&b.ask))
        })
}

#[derive(Clone)]
pub struct TradingAgentsOrchestrator {
    analysis: Analysis,
    llm: Llm,
    backend: Arc<dyn LlmBackend>,
    journal: Option<Arc<Journal>>,
}

impl TradingAgentsOrchestrator {
    pub fn new(analysis: Analysis, llm: Llm, backend: Arc<dyn LlmBackend>) -> Self {
        Self {
            analysis,
            llm,
            backend,
            journal: None,
        }
    }

    pub fn gemini(analysis: Analysis, llm: Llm, api_key: String) -> Self {
        let backend = Arc::new(GeminiLlmBackend::new(llm.clone(), api_key));
        Self::new(analysis, llm, backend)
    }

    pub fn deterministic(analysis: Analysis, llm: Llm) -> Self {
        Self::new(analysis, llm, Arc::new(DeterministicLlmBackend))
    }

    pub fn from_settings(settings: &Settings) -> Result<Self> {
        if settings.llm.use_offline_deterministic {
            return Ok(Self::deterministic(
                settings.analysis.clone(),
                settings.llm.clone(),
            ));
        }
        let key = std::env::var("GEMINI_API_KEY").context(
            "GEMINI_API_KEY not set; configure it or set llm.use_offline_deterministic = true",
        )?;
        Ok(Self::gemini(
            settings.analysis.clone(),
            settings.llm.clone(),
            key,
        ))
    }

    pub fn with_journal(mut self, journal: Arc<Journal>) -> Self {
        self.journal = Some(journal);
        self
    }

    async fn call_stage<T: for<'de> Deserialize<'de>>(
        &self,
        model: &str,
        system: &str,
        payload: &Value,
        max_tokens: u32,
    ) -> Result<T> {
        let timeout = Duration::from_secs(self.analysis.stage_timeout_seconds.max(1));
        let prompt = serde_json::to_string(payload)?;
        let value = tokio::time::timeout(
            timeout,
            self.backend.complete_json(model, system, &prompt, max_tokens),
        )
        .await
        .context("TradingAgents stage timed out")??;
        let stage = system.split(']').next().unwrap_or("STAGE");
        let normalized = normalize_stage_value(system, value);
        serde_json::from_value(normalized.clone())
            .with_context(|| format!("decode TradingAgents stage response ({stage}]): {normalized}"))
    }

    pub async fn decide_with_trace(
        &self,
        snapshot: &MarketSnapshot,
        position: Option<&PositionContext>,
        journal: Option<&Journal>,
    ) -> Result<PipelineTrace> {
        let active_journal = journal.or(self.journal.as_deref());
        let primary_symbol = position
            .map(|p| p.symbol.as_str())
            .or_else(|| snapshot.quotes.first().map(|q| q.symbol.as_str()));

        // Stage 0: Reflection memory retrieval
        let retrieved_memories: Vec<RetrievedMemory> =
            if self.analysis.reflection_enabled && self.analysis.max_memory_results > 0 {
                if let Some(j) = active_journal {
                    let tags = vec!["nifty".to_string(), "options".to_string()];
                    j.query_reflections(
                        &snapshot.context,
                        &tags,
                        primary_symbol,
                        self.analysis.max_memory_results,
                    )?
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

        // Stage 1: Analyst Team (3 parallel analysts)
        let analyst_payload = json!({
            "snapshot": snapshot,
            "memories": &retrieved_memories,
        });
        let (tech_res, opt_res, reg_res) = tokio::join!(
            self.call_stage::<AnalystReport>(
                &self.llm.quick_think_model,
                "[STAGE:ANALYST_TECHNICAL] You are the Technical Analyst for NIFTY options. Return JSON matching AnalystReport (role=\"technical\", stance, confidence_bps 0-10000, summary, key_levels, evidence, regime_tags).",
                &analyst_payload,
                self.llm.max_tokens_analyst,
            ),
            self.call_stage::<AnalystReport>(
                &self.llm.quick_think_model,
                "[STAGE:ANALYST_OPTIONS] You are the Options & Volatility Analyst for NIFTY. Evaluate IV, expiry, strike, and bid-ask spreads. Return JSON matching AnalystReport (role=\"options_volatility\", stance, confidence_bps, summary, key_levels, evidence, regime_tags).",
                &analyst_payload,
                self.llm.max_tokens_analyst,
            ),
            self.call_stage::<AnalystReport>(
                &self.llm.quick_think_model,
                "[STAGE:ANALYST_REGIME] You are the Regime & Sentiment Analyst for NIFTY. Assess liquidity, macro/event context, and recalled memories. Return JSON matching AnalystReport (role=\"regime_sentiment\", stance, confidence_bps, summary, key_levels, evidence, regime_tags).",
                &analyst_payload,
                self.llm.max_tokens_analyst,
            )
        );
        let analyst_reports = vec![tech_res?, opt_res?, reg_res?];

        // Stage 2: Research Team (Bull vs Bear dialectical debate + Research Manager)
        let debate_rounds = self.analysis.max_debate_rounds.max(1);
        let mut invest_debate = InvestDebateState::default();
        for round in 1..=debate_rounds {
            let bull_payload = json!({
                "round": round,
                "analyst_reports": &analyst_reports,
                "memories": &retrieved_memories,
                "debate": &invest_debate,
            });
            let bull_turn: DebateTurn = self
                .call_stage(
                    &self.llm.quick_think_model,
                    "[STAGE:RESEARCH_BULL] You are the Bull Researcher. Build the strongest bullish case from the analyst reports and memories, countering bear points. Return JSON DebateTurn (speaker, round, argument, confidence_bps).",
                    &bull_payload,
                    self.llm.max_tokens_debate,
                )
                .await?;
            invest_debate.bull_history.push(bull_turn.argument.clone());
            invest_debate.current_response = bull_turn.argument.clone();
            invest_debate.history.push(bull_turn);
            invest_debate.count += 1;

            let bear_payload = json!({
                "round": round,
                "analyst_reports": &analyst_reports,
                "memories": &retrieved_memories,
                "debate": &invest_debate,
            });
            let bear_turn: DebateTurn = self
                .call_stage(
                    &self.llm.quick_think_model,
                    "[STAGE:RESEARCH_BEAR] You are the Bear Researcher. Challenge the bull case with downside risks, theta decay, and invalidation scenarios. Return JSON DebateTurn (speaker, round, argument, confidence_bps).",
                    &bear_payload,
                    self.llm.max_tokens_debate,
                )
                .await?;
            invest_debate.bear_history.push(bear_turn.argument.clone());
            invest_debate.current_response = bear_turn.argument.clone();
            invest_debate.history.push(bear_turn);
            invest_debate.count += 1;
        }

        let manager_payload = json!({
            "analyst_reports": &analyst_reports,
            "invest_debate": &invest_debate,
            "memories": &retrieved_memories,
        });
        let research_decision: ResearchDecision = self
            .call_stage(
                &self.llm.deep_think_model,
                "[STAGE:RESEARCH_MANAGER] You are the Research Manager. Synthesize the Bull/Bear debate into one disciplined ResearchDecision JSON (stance, confidence_bps, thesis, investment_plan, preferred_right, key_risks).",
                &manager_payload,
                self.llm.max_tokens_debate,
            )
            .await?;

        // Stage 3: Trader Agent
        let trader_payload = json!({
            "snapshot": snapshot,
            "position": position,
            "research_decision": &research_decision,
            "memories": &retrieved_memories,
        });
        let trader_proposal: TraderProposal = self
            .call_stage(
                &self.llm.deep_think_model,
                "[STAGE:TRADER] You are the Trader Agent. From research_decision, position, and snapshot, return TraderProposal JSON (action, rationale, confidence_bps, signal). If flat and proposing BUY, copy ONE exact symbol verbatim from snapshot.quotes (never invent or combine symbols like CE+PE), quantity=1, and agent-selected option-premium entry_price, target_price, stop_loss_price, trailing_stop_loss_price satisfying 0 < stop_loss_price < trailing_stop_loss_price <= entry_price < target_price. Include signal fields: id, source, symbol, side, quantity, entry_price, target_price, stop_loss_price, trailing_stop_loss_price, confidence_bps, rationale. If holding, emit SELL only to close that symbol or signal=null to hold. Never short.",
                &trader_payload,
                self.llm.max_tokens_decision,
            )
            .await?;

        // Stage 4: Risk Management Committee (Aggressive, Conservative, Neutral + Risk Judge)
        let risk_rounds = self.analysis.max_risk_discuss_rounds.max(1);
        let mut risk_debate = RiskDebateState::default();
        for round in 1..=risk_rounds {
            for role in [
                RiskAnalystRole::Aggressive,
                RiskAnalystRole::Conservative,
                RiskAnalystRole::Neutral,
            ] {
                let risk_payload = json!({
                    "role": role.as_str(),
                    "round": round,
                    "trader_proposal": &trader_proposal,
                    "research_decision": &research_decision,
                    "risk_debate": &risk_debate,
                });
                let turn: DebateTurn = self
                    .call_stage(
                        &self.llm.quick_think_model,
                        "[STAGE:RISK_ANALYST] You are a Risk Analyst on the Risk Management Committee. Critique the trader_proposal from your assigned risk posture. Return JSON DebateTurn (speaker, round, argument, confidence_bps).",
                        &risk_payload,
                        self.llm.max_tokens_debate,
                    )
                    .await?;
                match role {
                    RiskAnalystRole::Aggressive => {
                        risk_debate.risky_history.push(turn.argument.clone());
                    }
                    RiskAnalystRole::Conservative => {
                        risk_debate.safe_history.push(turn.argument.clone());
                    }
                    RiskAnalystRole::Neutral => {
                        risk_debate.neutral_history.push(turn.argument.clone());
                    }
                }
                risk_debate.latest_speaker = role.as_str().into();
                risk_debate.history.push(turn);
                risk_debate.count += 1;
            }
        }

        let judge_payload = json!({
            "snapshot": snapshot,
            "position": position,
            "trader_proposal": &trader_proposal,
            "risk_debate": &risk_debate,
        });
        let mut risk_verdict: RiskVerdict = self
            .call_stage(
                &self.llm.deep_think_model,
                "[STAGE:RISK_MANAGER] You are the Risk Manager Judge. Review trader_proposal and risk_debate. Return RiskVerdict JSON (approved, decision, adjusted_confidence_bps, verdict_rationale, final_signal). Veto (approved=false, final_signal=null) if risk/reward or plan levels are unsafe.",
                &judge_payload,
                self.llm.max_tokens_decision,
            )
            .await?;

        // Deterministic safety enforcement on final_signal
        if !risk_verdict.approved {
            risk_verdict.final_signal = None;
        } else if let Some(ref mut sig) = risk_verdict.final_signal {
            sig.snapshot_id = Some(snapshot.id);
            if let Err(err) = risk::check_agent_plan(sig) {
                risk_verdict.approved = false;
                risk_verdict.decision = "VETO".into();
                risk_verdict.verdict_rationale =
                    format!("Deterministic plan check rejected signal: {err}");
                risk_verdict.final_signal = None;
            }
        }

        let final_signal = risk_verdict.final_signal.clone();
        let trace = PipelineTrace {
            snapshot_id: snapshot.id,
            created_at: snapshot.created_at,
            retrieved_memories,
            analyst_reports,
            invest_debate,
            research_decision,
            trader_proposal,
            risk_debate,
            risk_verdict,
            final_signal,
        };

        if let Some(j) = active_journal {
            j.record_pipeline_trace(&trace)?;
        }

        Ok(trace)
    }

    pub async fn reflect_on_trade(
        &self,
        outcome: &TradeOutcome,
        journal: Option<&Journal>,
    ) -> Result<TradeReflection> {
        self.reflect_on_trade_at(outcome, journal, Utc::now()).await
    }

    pub async fn reflect_on_trade_at(
        &self,
        outcome: &TradeOutcome,
        journal: Option<&Journal>,
        now: DateTime<Utc>,
    ) -> Result<TradeReflection> {
        #[derive(Deserialize)]
        struct ReflectorOutput {
            lesson_bull: String,
            lesson_bear: String,
            lesson_trader: String,
            lesson_risk: String,
            lesson_summary: String,
        }

        let active_journal = journal.or(self.journal.as_deref());
        let payload = json!({ "outcome": outcome });
        let out: ReflectorOutput = self
            .call_stage(
                &self.llm.deep_think_model,
                "[STAGE:REFLECTOR] You are the Post-Trade Reflector. Analyze the completed TradeOutcome and return JSON with lesson_bull, lesson_bear, lesson_trader, lesson_risk, lesson_summary.",
                &payload,
                self.llm.max_tokens_debate,
            )
            .await?;

        let mut reflection = TradeReflection {
            id: None,
            trade_id: outcome.trade_id.clone(),
            symbol: outcome.symbol.clone(),
            side: outcome.side,
            pnl_rupees: outcome.pnl_rupees,
            return_bps: outcome.return_bps,
            exit_reason: outcome.exit_reason.clone(),
            situation_summary: outcome.situation_summary.clone(),
            regime_tags: outcome.regime_tags.clone(),
            lesson_summary: out.lesson_summary,
            lesson_bull: out.lesson_bull,
            lesson_bear: out.lesson_bear,
            lesson_trader: out.lesson_trader,
            lesson_risk: out.lesson_risk,
            created_at: now,
        };

        if self.analysis.reflection_enabled {
            if let Some(j) = active_journal {
                let id = j.record_reflection(&reflection)?;
                reflection.id = Some(id);
            }
        }

        Ok(reflection)
    }
}

#[async_trait]
impl BacktestPolicy for TradingAgentsOrchestrator {
    async fn decide(
        &self,
        snapshot: &MarketSnapshot,
        position: Option<&BacktestPosition>,
    ) -> Result<Option<Signal>> {
        let (signal, _) = self.decide_traced(snapshot, position).await?;
        Ok(signal)
    }

    async fn decide_traced(
        &self,
        snapshot: &MarketSnapshot,
        position: Option<&BacktestPosition>,
    ) -> Result<(Option<Signal>, Option<crate::types::PipelineTrace>)> {
        let pos_ctx = position.map(|p| PositionContext {
            symbol: p.symbol.clone(),
            quantity: p.lots,
            entry_price: p.entry_price,
            target_price: p.target_price,
            stop_loss_price: p.stop_loss_price,
            trailing_stop_loss_price: p.trailing_stop_loss_price,
            unrealized_pnl_rupees: 0.0,
            entry_time: Some(p.entry_time),
        });
        let trace = self
            .decide_with_trace(snapshot, pos_ctx.as_ref(), self.journal.as_deref())
            .await?;
        let signal = trace.final_signal.clone();
        Ok((signal, Some(trace)))
    }

    async fn on_trade_closed(&self, outcome: &TradeOutcome) -> Result<()> {
        if self.analysis.reflection_enabled {
            self.reflect_on_trade_at(outcome, self.journal.as_deref(), outcome.exit_time)
                .await?;
        }
        Ok(())
    }
}

fn strip_markdown_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        rest.trim().strip_suffix("```").unwrap_or(rest).trim()
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest.trim().strip_suffix("```").unwrap_or(rest).trim()
    } else {
        trimmed
    }
}

fn normalize_stage_value(system: &str, mut value: Value) -> Value {
    if let Value::Array(mut arr) = value {
        value = if arr.len() == 1 {
            arr.remove(0)
        } else {
            Value::Object(serde_json::Map::new())
        };
    }
    let Value::Object(mut map) = value else {
        return value;
    };
    if map.len() == 1 {
        if let Some((_, inner)) = map.iter().next() {
            if inner.is_object() {
                if let Some((_, Value::Object(inner_map))) = map.into_iter().next() {
                    map = inner_map;
                } else {
                    unreachable!();
                }
            }
        }
    }

    if system.contains("[STAGE:ANALYST_TECHNICAL]") {
        map.insert("role".into(), json!("technical"));
    } else if system.contains("[STAGE:ANALYST_OPTIONS]") {
        map.insert("role".into(), json!("options_volatility"));
    } else if system.contains("[STAGE:ANALYST_REGIME]") {
        map.insert("role".into(), json!("regime_sentiment"));
    }

    for key in [
        "stance",
        "summary",
        "evidence",
        "speaker",
        "argument",
        "thesis",
        "investment_plan",
        "action",
        "rationale",
        "decision",
        "verdict_rationale",
        "lesson_bull",
        "lesson_bear",
        "lesson_trader",
        "lesson_risk",
        "lesson_summary",
    ] {
        if let Some(v) = map.get_mut(key) {
            *v = json!(value_to_string(v));
        }
    }

    if let Some(v) = map.get_mut("preferred_right") {
        if !v.is_null() {
            *v = json!(value_to_string(v));
        }
    }

    for key in ["key_levels", "regime_tags", "key_risks"] {
        if let Some(v) = map.get_mut(key) {
            *v = Value::Array(
                value_to_string_list(v)
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            );
        }
    }

    for key in ["confidence_bps", "adjusted_confidence_bps"] {
        if let Some(v) = map.get_mut(key) {
            *v = json!(value_to_u64(v, 5000).min(10_000));
        }
    }
    if let Some(v) = map.get_mut("round") {
        *v = json!(value_to_u64(v, 1).max(1));
    }

    let parent_conf = map
        .get("confidence_bps")
        .or_else(|| map.get("adjusted_confidence_bps"))
        .and_then(|v| v.as_u64())
        .unwrap_or(5000);
    let parent_rationale = map
        .get("rationale")
        .or_else(|| map.get("verdict_rationale"))
        .map(value_to_string)
        .unwrap_or_default();

    for sig_key in ["signal", "final_signal"] {
        if let Some(v) = map.get_mut(sig_key) {
            normalize_signal_value(v, parent_conf, &parent_rationale);
        }
    }

    Value::Object(map)
}

fn normalize_signal_value(value: &mut Value, parent_conf: u64, parent_rationale: &str) {
    let Value::Object(map) = value else {
        return;
    };
    if !map.contains_key("id") || value_to_string(&map["id"]).trim().is_empty() {
        map.insert("id".into(), json!(format!("ta-{}", uuid::Uuid::new_v4())));
    }
    if !map.contains_key("source") || value_to_string(&map["source"]).trim().is_empty() {
        map.insert("source".into(), json!("trading_agents_trader"));
    }
    if !map.contains_key("rationale") || value_to_string(&map["rationale"]).trim().is_empty() {
        map.insert("rationale".into(), json!(parent_rationale));
    }
    if !map.contains_key("confidence_bps") {
        map.insert("confidence_bps".into(), json!(parent_conf.min(10_000)));
    }
    if !map.contains_key("quantity") {
        map.insert("quantity".into(), json!(1));
    }
    if !map.contains_key("snapshot_id") {
        map.insert("snapshot_id".into(), Value::Null);
    }
    if let Some(side) = map.get_mut("side") {
        let s = value_to_string(side).to_ascii_lowercase();
        *side = if s.contains("sell") {
            json!("sell")
        } else {
            json!("buy")
        };
    } else {
        map.insert("side".into(), json!("buy"));
    }
    for key in ["id", "source", "symbol", "rationale"] {
        if let Some(v) = map.get_mut(key) {
            *v = json!(value_to_string(v));
        }
    }
    if let Some(v) = map.get_mut("quantity") {
        *v = json!(value_to_u64(v, 1).max(1));
    }
    if let Some(v) = map.get_mut("confidence_bps") {
        *v = json!(value_to_u64(v, parent_conf).min(10_000));
    }
    if let Some(v) = map.get_mut("snapshot_id") {
        let ok = v
            .as_str()
            .and_then(|s| uuid::Uuid::parse_str(s.trim()).ok())
            .is_some();
        if !ok {
            *v = Value::Null;
        }
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn value_to_string_list(v: &Value) -> Vec<String> {
    match v {
        Value::Null => Vec::new(),
        Value::String(s) => {
            if s.trim().is_empty() {
                Vec::new()
            } else {
                vec![s.clone()]
            }
        }
        Value::Array(items) => items
            .iter()
            .filter(|item| !item.is_null())
            .map(value_to_string)
            .collect(),
        Value::Object(obj) => obj
            .iter()
            .map(|(k, val)| format!("{k}: {}", value_to_string(val)))
            .collect(),
        other => vec![value_to_string(other)],
    }
}

fn value_to_u64(v: &Value, default: u64) -> u64 {
    if let Some(u) = v.as_u64() {
        return u;
    }
    if let Some(f) = v.as_f64() {
        if f.is_finite() && f >= 0.0 {
            return f.round() as u64;
        }
    }
    if let Some(s) = v.as_str() {
        if let Ok(parsed) = s.trim().parse::<f64>() {
            if parsed.is_finite() && parsed >= 0.0 {
                return parsed.round() as u64;
            }
        }
    }
    default
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Quote;
    use chrono::Utc;
    use uuid::Uuid;

    fn sample_snapshot() -> MarketSnapshot {
        let now = Utc::now();
        MarketSnapshot {
            id: Uuid::new_v4(),
            created_at: now,
            quotes: vec![
                Quote {
                    symbol: "NIFTY-2026-11-24-25350-CE".into(),
                    bid: 120.0,
                    ask: 121.0,
                    last: 120.5,
                    timestamp: now,
                },
                Quote {
                    symbol: "NIFTY-2026-11-24-25350-PE".into(),
                    bid: 115.0,
                    ask: 117.0,
                    last: 116.0,
                    timestamp: now,
                },
            ],
            context: "HISTORICAL NIFTY BAR; spot O/H/L/C=25300/25360/25290/25350; FLAT".into(),
        }
    }

    #[tokio::test]
    async fn trading_agents_pipeline_runs_all_five_stages_deterministically() {
        let orch = TradingAgentsOrchestrator::deterministic(Analysis::default(), Llm::default());
        let journal = Journal::open(":memory:").unwrap();
        let snap = sample_snapshot();

        let trace = orch.decide_with_trace(&snap, None, Some(&journal)).await.unwrap();
        assert_eq!(trace.analyst_reports.len(), 3);
        assert_eq!(trace.invest_debate.bull_history.len(), 1);
        assert_eq!(trace.invest_debate.bear_history.len(), 1);
        assert_eq!(trace.risk_debate.risky_history.len(), 1);
        assert_eq!(trace.risk_debate.safe_history.len(), 1);
        assert_eq!(trace.risk_debate.neutral_history.len(), 1);
        assert!(trace.risk_verdict.approved);
        let sig = trace.final_signal.expect("expected buy signal");
        assert_eq!(sig.side, Side::Buy);
        assert!(risk::check_agent_plan(&sig).is_ok());

        let outcome = TradeOutcome {
            trade_id: sig.id.clone(),
            symbol: sig.symbol.clone(),
            side: Side::Buy,
            quantity: sig.quantity,
            entry_price: 121.0,
            exit_price: 145.0,
            pnl_rupees: 24.0 * 65.0,
            return_bps: 1983,
            exit_reason: "target_hit".into(),
            entry_time: Utc::now(),
            exit_time: Utc::now(),
            situation_summary: snap.context.clone(),
            regime_tags: vec!["nifty".into(), "options".into()],
            decision_rationale: sig.rationale.clone(),
        };
        let reflection = orch.reflect_on_trade(&outcome, Some(&journal)).await.unwrap();
        assert!(reflection.id.is_some());
        assert_eq!(journal.reflection_count().unwrap(), 1);

        // Next decision should recall the persisted reflection memory
        let snap2 = sample_snapshot();
        let trace2 = orch
            .decide_with_trace(&snap2, None, Some(&journal))
            .await
            .unwrap();
        assert_eq!(trace2.retrieved_memories.len(), 1);
    }

    #[test]
    fn normalizes_messy_llm_json_maps_and_casings() {
        let raw_analyst = json!({
            "role": "Technical Analyst",
            "stance": "bullish",
            "confidence_bps": 7250.0,
            "summary": "Momentum up",
            "key_levels": { "support": 25300, "resistance": 25450 },
            "evidence": { "spot": 25350 },
            "regime_tags": "intraday"
        });
        let norm = normalize_stage_value("[STAGE:ANALYST_TECHNICAL]", raw_analyst);
        let report: AnalystReport = serde_json::from_value(norm).unwrap();
        assert_eq!(report.role, AnalystRole::Technical);
        assert_eq!(report.confidence_bps, 7250);
        assert_eq!(report.key_levels.len(), 2);
        assert_eq!(report.regime_tags, vec!["intraday"]);

        let raw_trader = json!({
            "action": "BUY",
            "rationale": "Breakout",
            "confidence_bps": "7500",
            "signal": {
                "id": "sig-1",
                "source": "trader",
                "symbol": "NIFTY-2026-11-24-25350-CE",
                "side": "BUY",
                "quantity": 1.0,
                "entry_price": 120.0,
                "target_price": 150.0,
                "stop_loss_price": 90.0,
                "trailing_stop_loss_price": 105.0,
                "confidence_bps": 7500,
                "rationale": "Breakout",
                "snapshot_id": ""
            }
        });
        let norm_t = normalize_stage_value("[STAGE:TRADER]", raw_trader);
        let proposal: TraderProposal = serde_json::from_value(norm_t).unwrap();
        let sig = proposal.signal.unwrap();
        assert_eq!(sig.side, Side::Buy);
        assert!(sig.snapshot_id.is_none());

        // Exact minimal signal shape returned by Gemini (omits id, source, side, confidence_bps, rationale, snapshot_id)
        let minimal_trader = json!({
            "action": "BUY",
            "confidence_bps": 6000,
            "rationale": "Range trade",
            "signal": {
                "entry_price": 120.0,
                "quantity": 1,
                "stop_loss_price": 90.0,
                "symbol": "NIFTY-2026-09-01-24350-CE",
                "target_price": 150.0,
                "trailing_stop_loss_price": 105.0
            }
        });
        let norm_min = normalize_stage_value("[STAGE:TRADER]", minimal_trader);
        let proposal_min: TraderProposal = serde_json::from_value(norm_min).unwrap();
        let sig_min = proposal_min.signal.unwrap();
        assert!(sig_min.id.starts_with("ta-"));
        assert_eq!(sig_min.source, "trading_agents_trader");
        assert_eq!(sig_min.confidence_bps, 6000);
        assert_eq!(sig_min.rationale, "Range trade");
    }

    #[tokio::test]
    async fn reflect_on_trade_at_stamps_injected_timestamp() {
        use chrono::TimeZone;
        let orch = TradingAgentsOrchestrator::deterministic(Analysis::default(), Llm::default());
        let journal = Journal::open(":memory:").unwrap();
        let sim_now = Utc.with_ymd_and_hms(2025, 2, 3, 9, 45, 0).unwrap();
        let outcome = TradeOutcome {
            trade_id: "sim-trade-1".into(),
            symbol: "NIFTY-25000-CE".into(),
            side: Side::Buy,
            quantity: 10,
            entry_price: 100.0,
            exit_price: 120.0,
            pnl_rupees: 200.0,
            return_bps: 2000,
            exit_reason: "target_hit".into(),
            entry_time: sim_now - chrono::Duration::minutes(10),
            exit_time: sim_now,
            situation_summary: "simulated".into(),
            regime_tags: vec!["nifty".into()],
            decision_rationale: "test".into(),
        };
        let reflection = orch
            .reflect_on_trade_at(&outcome, Some(&journal), sim_now)
            .await
            .unwrap();
        assert_eq!(reflection.created_at, sim_now);
        let stored = journal.recent_reflections(1).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].created_at, sim_now);
    }
}

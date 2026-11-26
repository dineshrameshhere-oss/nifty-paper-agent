use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Quote {
    pub symbol: String,
    pub bid: f64,
    pub ask: f64,
    pub last: f64,
    pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketSnapshot {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub quotes: Vec<Quote>,
    pub context: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Signal {
    pub id: String,
    pub source: String,
    pub symbol: String,
    pub side: Side,
    pub quantity: u32,
    /// Agent-selected expected option premium entry, used as a hard price cap for buys.
    pub entry_price: f64,
    /// Agent-selected option-premium exit levels.
    pub target_price: f64,
    pub stop_loss_price: f64,
    pub trailing_stop_loss_price: f64,
    pub confidence_bps: u16,
    pub rationale: String,
    pub snapshot_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fill {
    pub signal_id: String,
    pub symbol: String,
    pub side: Side,
    pub quantity: u32,
    pub price: f64,
    pub fees: f64,
    pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalystRole {
    Technical,
    OptionsVolatility,
    RegimeSentiment,
}

impl AnalystRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Technical => "technical",
            Self::OptionsVolatility => "options_volatility",
            Self::RegimeSentiment => "regime_sentiment",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalystReport {
    pub role: AnalystRole,
    pub stance: String,
    pub confidence_bps: u16,
    pub summary: String,
    #[serde(default)]
    pub key_levels: Vec<String>,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub regime_tags: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebateTurn {
    pub speaker: String,
    pub round: usize,
    pub argument: String,
    #[serde(default)]
    pub confidence_bps: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvestDebateState {
    #[serde(default)]
    pub bull_history: Vec<String>,
    #[serde(default)]
    pub bear_history: Vec<String>,
    #[serde(default)]
    pub history: Vec<DebateTurn>,
    #[serde(default)]
    pub current_response: String,
    #[serde(default)]
    pub count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchDecision {
    pub stance: String,
    pub confidence_bps: u16,
    pub thesis: String,
    pub investment_plan: String,
    #[serde(default)]
    pub preferred_right: Option<String>,
    #[serde(default)]
    pub key_risks: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PositionContext {
    pub symbol: String,
    pub quantity: u32,
    pub entry_price: f64,
    pub target_price: f64,
    pub stop_loss_price: f64,
    pub trailing_stop_loss_price: f64,
    #[serde(default)]
    pub unrealized_pnl_rupees: f64,
    #[serde(default)]
    pub entry_time: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraderProposal {
    pub action: String,
    pub rationale: String,
    pub confidence_bps: u16,
    #[serde(default)]
    pub signal: Option<Signal>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskAnalystRole {
    Aggressive,
    Conservative,
    Neutral,
}

impl RiskAnalystRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Aggressive => "aggressive",
            Self::Conservative => "conservative",
            Self::Neutral => "neutral",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskDebateState {
    #[serde(default)]
    pub risky_history: Vec<String>,
    #[serde(default)]
    pub safe_history: Vec<String>,
    #[serde(default)]
    pub neutral_history: Vec<String>,
    #[serde(default)]
    pub history: Vec<DebateTurn>,
    #[serde(default)]
    pub latest_speaker: String,
    #[serde(default)]
    pub count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskVerdict {
    pub approved: bool,
    pub decision: String,
    pub adjusted_confidence_bps: u16,
    pub verdict_rationale: String,
    #[serde(default)]
    pub final_signal: Option<Signal>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PipelineTrace {
    pub snapshot_id: Uuid,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub retrieved_memories: Vec<RetrievedMemory>,
    pub analyst_reports: Vec<AnalystReport>,
    pub invest_debate: InvestDebateState,
    pub research_decision: ResearchDecision,
    pub trader_proposal: TraderProposal,
    pub risk_debate: RiskDebateState,
    pub risk_verdict: RiskVerdict,
    #[serde(default)]
    pub final_signal: Option<Signal>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TradeOutcome {
    pub trade_id: String,
    pub symbol: String,
    pub side: Side,
    pub quantity: u32,
    pub entry_price: f64,
    pub exit_price: f64,
    pub pnl_rupees: f64,
    pub return_bps: i32,
    pub exit_reason: String,
    pub entry_time: DateTime<Utc>,
    pub exit_time: DateTime<Utc>,
    #[serde(default)]
    pub situation_summary: String,
    #[serde(default)]
    pub regime_tags: Vec<String>,
    #[serde(default)]
    pub decision_rationale: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TradeReflection {
    #[serde(default)]
    pub id: Option<i64>,
    pub trade_id: String,
    pub symbol: String,
    pub side: Side,
    pub pnl_rupees: f64,
    pub return_bps: i32,
    #[serde(default)]
    pub exit_reason: String,
    pub situation_summary: String,
    #[serde(default)]
    pub regime_tags: Vec<String>,
    pub lesson_summary: String,
    #[serde(default)]
    pub lesson_bull: String,
    #[serde(default)]
    pub lesson_bear: String,
    #[serde(default)]
    pub lesson_trader: String,
    #[serde(default)]
    pub lesson_risk: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetrievedMemory {
    pub reflection: TradeReflection,
    pub relevance_score: f64,
    #[serde(default)]
    pub matched_tags: Vec<String>,
}

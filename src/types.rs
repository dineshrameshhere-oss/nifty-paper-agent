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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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
    pub limit_price: f64,
    pub confidence_bps: u16,
    pub rationale: String,
    pub snapshot_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentOpinion {
    pub objective: String,
    pub stance: String,
    pub confidence_bps: u16,
    pub evidence: String,
    pub signal: Option<Signal>,
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

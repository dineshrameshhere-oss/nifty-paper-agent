use anyhow::{Context, Result};
use serde::Deserialize;
use std::{fs, path::Path};

#[derive(Clone, Debug, Deserialize)]
pub struct Settings {
    pub paper: Paper,
    pub analysis: Analysis,
    pub llm: Llm,
    pub indstocks: Indstocks,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Paper {
    pub starting_capital_paise: i64,
    pub max_trade_value_paise: i64,
    pub max_open_positions: usize,
    pub max_daily_loss_paise: i64,
    pub max_position_quantity: u32,
    pub allow_short: bool,
    pub spread_bps: u32,
    pub slippage_bps: u32,
    pub fee_bps: u32,
    pub partial_fill_fraction: f64,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Analysis {
    pub mode: String,
    pub bar_minutes: u32,
    pub stale_after_seconds: i64,
    pub worker_count: usize,
    pub subleader_count: usize,
    pub driver_count: usize,
    pub worker_timeout_seconds: u64,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Llm {
    pub base_url: String,
    pub worker_model: String,
    pub subleader_model: String,
    pub driver_model: String,
    pub max_concurrent_requests: usize,
    pub requests_per_minute: usize,
    pub max_tokens_worker: u32,
    pub max_tokens_subleader: u32,
    pub max_tokens_driver: u32,
    pub temperature: f32,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Indstocks {
    pub rest_base_url: String,
    pub websocket_url: String,
    pub quote_path: String,
    pub market_data_enabled: bool,
    pub symbol: String,
}
impl Settings {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref()).context("read settings.toml")?;
        toml::from_str(&content).context("parse settings.toml")
    }
}

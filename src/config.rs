use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    pub paper: Paper,
    pub analysis: Analysis,
    pub llm: Llm,
    pub indstocks: Indstocks,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

impl Default for Paper {
    fn default() -> Self {
        Self {
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Analysis {
    pub mode: String,
    pub bar_minutes: u64,
    pub stale_after_seconds: i64,
    #[serde(default = "default_one")]
    pub max_debate_rounds: usize,
    #[serde(default = "default_one")]
    pub max_risk_discuss_rounds: usize,
    #[serde(default = "default_true")]
    pub reflection_enabled: bool,
    #[serde(default = "default_five")]
    pub max_memory_results: usize,
    #[serde(default = "default_thirty")]
    pub stage_timeout_seconds: u64,
}

impl Default for Analysis {
    fn default() -> Self {
        Self {
            mode: "hybrid".into(),
            bar_minutes: 5,
            stale_after_seconds: 15,
            max_debate_rounds: default_one(),
            max_risk_discuss_rounds: default_one(),
            reflection_enabled: default_true(),
            max_memory_results: default_five(),
            stage_timeout_seconds: default_thirty(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Llm {
    pub base_url: String,
    #[serde(default = "default_flash_lite")]
    pub quick_think_model: String,
    #[serde(default = "default_flash_lite")]
    pub deep_think_model: String,
    pub fallback_model: String,
    pub max_concurrent_requests: usize,
    pub requests_per_minute: u32,
    #[serde(default = "default_tokens_analyst")]
    pub max_tokens_analyst: u32,
    #[serde(default = "default_tokens_debate")]
    pub max_tokens_debate: u32,
    #[serde(default = "default_tokens_decision")]
    pub max_tokens_decision: u32,
    pub temperature: f32,
    #[serde(default)]
    pub use_offline_deterministic: bool,
}

impl Default for Llm {
    fn default() -> Self {
        Self {
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
                .into(),
            quick_think_model: default_flash_lite(),
            deep_think_model: default_flash_lite(),
            fallback_model: "gemini-3.5-flash-lite".into(),
            max_concurrent_requests: 10,
            requests_per_minute: 5,
            max_tokens_analyst: default_tokens_analyst(),
            max_tokens_debate: default_tokens_debate(),
            max_tokens_decision: default_tokens_decision(),
            temperature: 0.2,
            use_offline_deterministic: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Indstocks {
    pub rest_base_url: String,
    pub websocket_url: String,
    pub quote_path: String,
    #[serde(default = "default_option_chain_path")]
    pub option_chain_path: String,
    #[serde(default = "default_expiries_path")]
    pub expiries_path: String,
    #[serde(default = "default_exchange")]
    pub exchange: String,
    #[serde(default = "default_segment")]
    pub segment: String,
    #[serde(default = "default_underlying_scrip")]
    pub underlying_scrip: String,
    #[serde(default = "default_strike_count")]
    pub strike_count: u32,
    #[serde(default = "default_poll_interval_seconds")]
    pub poll_interval_seconds: u64,
    #[serde(default)]
    pub expiry_override: Option<String>,
    pub market_data_enabled: bool,
    pub symbol: String,
}

impl Default for Indstocks {
    fn default() -> Self {
        Self {
            rest_base_url: "https://api.indstocks.com".into(),
            websocket_url: "wss://ws-prices.indstocks.com/api/v1/ws/prices".into(),
            quote_path: "/market/quotes/full".into(),
            option_chain_path: default_option_chain_path(),
            expiries_path: default_expiries_path(),
            exchange: default_exchange(),
            segment: default_segment(),
            underlying_scrip: default_underlying_scrip(),
            strike_count: default_strike_count(),
            poll_interval_seconds: default_poll_interval_seconds(),
            expiry_override: None,
            market_data_enabled: false,
            symbol: "NIFTY".into(),
        }
    }
}

impl Settings {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref()).context("read settings.toml")?;
        toml::from_str(&content).context("parse settings.toml")
    }
}

fn default_one() -> usize {
    1
}
fn default_five() -> usize {
    5
}
fn default_thirty() -> u64 {
    30
}
fn default_true() -> bool {
    true
}
fn default_flash_lite() -> String {
    "gemini-3.1-flash-lite".into()
}
fn default_tokens_analyst() -> u32 {
    500
}
fn default_tokens_debate() -> u32 {
    600
}
fn default_tokens_decision() -> u32 {
    800
}
fn default_option_chain_path() -> String {
    "/market/option-chain".into()
}
fn default_expiries_path() -> String {
    "/market/instruments/expiries".into()
}
fn default_exchange() -> String {
    "NSE".into()
}
fn default_segment() -> String {
    "INDEX".into()
}
fn default_underlying_scrip() -> String {
    "40000001".into()
}
fn default_strike_count() -> u32 {
    10
}
fn default_poll_interval_seconds() -> u64 {
    5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_settings_toml_with_trading_agents_fields() {
        let manifest_settings = Path::new(env!("CARGO_MANIFEST_DIR")).join("settings.toml");
        let s = Settings::load(&manifest_settings).unwrap();
        if Path::new("settings.toml").exists() {
            let rel = Settings::load("settings.toml").unwrap();
            assert_eq!(rel.analysis.mode, s.analysis.mode);
        }

        assert_eq!(s.analysis.mode, "hybrid");
        assert_eq!(s.analysis.bar_minutes, 5);
        assert_eq!(s.analysis.stale_after_seconds, 15);
        assert_eq!(s.analysis.max_debate_rounds, 1);
        assert_eq!(s.analysis.max_risk_discuss_rounds, 1);
        assert!(s.analysis.reflection_enabled);
        assert_eq!(s.analysis.max_memory_results, 5);
        assert_eq!(s.analysis.stage_timeout_seconds, 30);

        assert_eq!(s.llm.quick_think_model, "gemini-3.1-flash-lite");
        assert_eq!(s.llm.deep_think_model, "gemini-3.1-flash-lite");
        assert_eq!(s.llm.fallback_model, "gemini-3.5-flash-lite");
        assert_eq!(s.llm.max_tokens_analyst, 500);
        assert_eq!(s.llm.max_tokens_debate, 600);
        assert_eq!(s.llm.max_tokens_decision, 800);
        assert_eq!(s.llm.requests_per_minute, 5);
        assert!(!s.llm.use_offline_deterministic);
    }

    #[test]
    fn omitted_optional_and_legacy_fields_use_expected_defaults() {
        let minimal = r#"
[paper]
starting_capital_paise = 1000000
max_trade_value_paise = 200000
max_open_positions = 3
max_daily_loss_paise = 50000
max_position_quantity = 50
allow_short = false
spread_bps = 10
slippage_bps = 5
fee_bps = 3
partial_fill_fraction = 1.0

[analysis]
mode = "hybrid"
bar_minutes = 5
stale_after_seconds = 15

[llm]
base_url = "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
fallback_model = "gemini-3.5-flash-lite"
max_concurrent_requests = 10
requests_per_minute = 5
temperature = 0.2

[indstocks]
rest_base_url = "https://example.invalid"
websocket_url = "wss://example.invalid"
quote_path = "/quote"
market_data_enabled = false
symbol = "NIFTY"
"#;

        let s: Settings = toml::from_str(minimal).unwrap();
        assert_eq!(s.analysis.max_debate_rounds, 1);
        assert_eq!(s.analysis.max_risk_discuss_rounds, 1);
        assert!(s.analysis.reflection_enabled);
        assert_eq!(s.analysis.max_memory_results, 5);
        assert_eq!(s.analysis.stage_timeout_seconds, 30);

        assert_eq!(s.llm.quick_think_model, "gemini-3.1-flash-lite");
        assert_eq!(s.llm.deep_think_model, "gemini-3.1-flash-lite");
        assert_eq!(s.llm.max_tokens_analyst, 500);
        assert_eq!(s.llm.max_tokens_debate, 600);
        assert_eq!(s.llm.max_tokens_decision, 800);
        assert!(!s.llm.use_offline_deterministic);

        assert_eq!(s.indstocks.option_chain_path, "/market/option-chain");
        assert_eq!(s.indstocks.expiries_path, "/market/instruments/expiries");
        assert_eq!(s.indstocks.exchange, "NSE");
        assert_eq!(s.indstocks.segment, "INDEX");
        assert_eq!(s.indstocks.underlying_scrip, "40000001");
        assert_eq!(s.indstocks.strike_count, 10);
        assert_eq!(s.indstocks.poll_interval_seconds, 5);
        assert_eq!(s.indstocks.expiry_override, None);
    }

    #[test]
    fn use_offline_deterministic_and_reflection_overrides_parse() {
        let raw = r#"
[paper]
starting_capital_paise = 1000000
max_trade_value_paise = 200000
max_open_positions = 3
max_daily_loss_paise = 50000
max_position_quantity = 50
allow_short = false
spread_bps = 10
slippage_bps = 5
fee_bps = 3
partial_fill_fraction = 1.0

[analysis]
mode = "hybrid"
bar_minutes = 5
stale_after_seconds = 15
reflection_enabled = false
max_debate_rounds = 2
max_risk_discuss_rounds = 2

[llm]
base_url = "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
fallback_model = "gemini-3.5-flash-lite"
max_concurrent_requests = 10
requests_per_minute = 5
temperature = 0.2
use_offline_deterministic = true

[indstocks]
rest_base_url = "https://example.invalid"
websocket_url = "wss://example.invalid"
quote_path = "/quote"
market_data_enabled = false
symbol = "NIFTY"
"#;

        let s: Settings = toml::from_str(raw).unwrap();
        assert!(!s.analysis.reflection_enabled);
        assert_eq!(s.analysis.max_debate_rounds, 2);
        assert_eq!(s.analysis.max_risk_discuss_rounds, 2);
        assert!(s.llm.use_offline_deterministic);
    }

    #[test]
    fn settings_default_has_non_placeholder_models_and_valid_counts() {
        let s = Settings::default();
        assert!(!s.llm.quick_think_model.starts_with("SET_"));
        assert!(!s.llm.deep_think_model.starts_with("SET_"));
        assert!(!s.llm.fallback_model.starts_with("SET_"));
        assert!(s.analysis.max_debate_rounds >= 1);
        assert!(s.analysis.max_risk_discuss_rounds >= 1);
        assert!(s.analysis.reflection_enabled);
        assert!(!s.indstocks.market_data_enabled);
    }

    #[test]
    fn loads_verified_indstocks_settings_from_toml() {
        let manifest_settings = Path::new(env!("CARGO_MANIFEST_DIR")).join("settings.toml");
        let s = Settings::load(&manifest_settings).unwrap();

        assert_eq!(s.indstocks.rest_base_url, "https://api.indstocks.com");
        assert_eq!(
            s.indstocks.websocket_url,
            "wss://ws-prices.indstocks.com/api/v1/ws/prices"
        );
        assert_eq!(s.indstocks.quote_path, "/market/quotes/full");
        assert_eq!(s.indstocks.option_chain_path, "/market/option-chain");
        assert_eq!(s.indstocks.expiries_path, "/market/instruments/expiries");
        assert_eq!(s.indstocks.exchange, "NSE");
        assert_eq!(s.indstocks.segment, "INDEX");
        assert_eq!(s.indstocks.underlying_scrip, "40000001");
        assert_eq!(s.indstocks.strike_count, 10);
        assert_eq!(s.indstocks.poll_interval_seconds, 5);
        assert_eq!(s.indstocks.expiry_override, None);
        assert!(s.indstocks.market_data_enabled);
        assert_eq!(s.indstocks.symbol, "NIFTY");
    }
}

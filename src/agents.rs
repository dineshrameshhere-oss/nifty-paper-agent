use crate::{
    config::Llm,
    types::{AgentOpinion, MarketSnapshot},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, Semaphore},
    time::Instant,
};

#[derive(Clone)]
pub struct GeminiOrchestrator {
    client: reqwest::Client,
    config: Llm,
    key: String,
    gate: Arc<Semaphore>,
    last_request: Arc<Mutex<Option<Instant>>>,
}
#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: [Message<'a>; 1],
    temperature: f32,
    max_tokens: u32,
    reasoning_effort: &'static str,
    response_format: Format,
}
#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}
#[derive(Serialize)]
struct Format {
    #[serde(rename = "type")]
    kind: &'static str,
}
#[derive(Deserialize)]
struct Response {
    choices: Vec<Choice>,
}
#[derive(Deserialize)]
struct Choice {
    message: Content,
}
#[derive(Deserialize)]
struct Content {
    content: String,
}

impl GeminiOrchestrator {
    pub fn new(config: Llm, key: String) -> Self {
        let permits = config.max_concurrent_requests.max(1);
        Self {
            client: reqwest::Client::new(),
            config,
            key,
            gate: Arc::new(Semaphore::new(permits)),
            last_request: Arc::new(Mutex::new(None)),
        }
    }
    async fn complete<T: for<'de> Deserialize<'de>>(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<T> {
        let _permit = self.gate.acquire().await?;
        // Simple global spacing limiter: safe under the configured RPM, though it does not burst.
        let min_gap = Duration::from_secs_f64(60.0 / self.config.requests_per_minute.max(1) as f64);
        let mut last = self.last_request.lock().await;
        if let Some(at) = *last {
            tokio::time::sleep(min_gap.saturating_sub(at.elapsed())).await;
        }
        *last = Some(Instant::now());
        drop(last);
        let user_prompt = format!(
            "{system}\n\nReturn exactly one valid JSON object and no Markdown. Include these fields: objective (string), stance (string), confidence_bps (integer from 0 to 10000), evidence (string), and signal (object or null). If signal is an object, include id, source, symbol, side, quantity, limit_price, confidence_bps, rationale, and snapshot_id."
        ) + &format!("\n\nInput and evidence:\n{prompt}");
        let body = Request {
            model,
            messages: [Message {
                role: "user",
                content: &user_prompt,
            }],
            temperature: self.config.temperature,
            max_tokens,
            reasoning_effort: "minimal",
            response_format: Format {
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
        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            bail!("Gemini API returned HTTP {status}: {error_body}");
        }
        let response: Response = response.json().await.context("decode Gemini response")?;
        let json = response
            .choices
            .first()
            .context("Gemini returned no choices")?
            .message
            .content
            .trim();
        serde_json::from_str(json).context("parse structured agent response")
    }
    async fn analyze_with_driver(
        &self,
        snapshot: &MarketSnapshot,
        driver_instruction: &str,
    ) -> Result<AgentOpinion> {
        if self.config.worker_model.starts_with("SET_")
            || self.config.subleader_model.starts_with("SET_")
            || self.config.driver_model.starts_with("SET_")
        {
            bail!("configure Gemini model IDs in settings.toml")
        }
        let parts = partition(snapshot);
        let mut jobs = Vec::new();
        for (index, part) in parts.into_iter().enumerate() {
            let this = self.clone();
            let objective = "Assess the NIFTY market evidence for a paper-trading opportunity. Return objective, stance, confidence_bps (0-10000), evidence, and signal=null. Do not invent prices or trade instructions.";
            jobs.push(tokio::spawn(async move {
                this.complete::<AgentOpinion>(
                    &this.config.worker_model,
                    objective,
                    &format!("worker={} partition={}", index + 1, part),
                    this.config.max_tokens_worker,
                )
                .await
            }));
        }
        let mut opinions = Vec::with_capacity(10);
        for job in jobs {
            opinions.push(job.await??);
        }
        require_complete_cycle(&opinions, 10)?;
        let a = self
            .lead(
                "subleader A: synthesize workers 1-5 without adding claims",
                &opinions[..5],
                &self.config.subleader_model,
                self.config.max_tokens_subleader,
            )
            .await?;
        let b = self
            .lead(
                "subleader B: synthesize workers 6-10 without adding claims",
                &opinions[5..],
                &self.config.subleader_model,
                self.config.max_tokens_subleader,
            )
            .await?;
        let final_input = serde_json::json!({"snapshot":snapshot,"subleaders":[a,b]});
        self.complete(
            &self.config.driver_model,
            driver_instruction,
            &final_input.to_string(),
            self.config.max_tokens_driver,
        )
        .await
    }
    pub async fn analyze(&self, snapshot: &MarketSnapshot) -> Result<AgentOpinion> {
        self.analyze_with_driver(snapshot,"Driver: synthesize both subleaders into one final assessment. Preserve signal=null; the live starter has no autonomous order authorization.").await
    }
    pub async fn analyze_backtest(&self, snapshot: &MarketSnapshot) -> Result<AgentOpinion> {
        self.analyze_with_driver(snapshot,"Driver: make a paper-backtest decision from the historical snapshot. If flat, optionally emit one BUY signal for an exact quoted synthetic NIFTY option symbol; quantity is one lot and limit_price must be no higher than that quote's ask. If already holding, only emit SELL to close that same symbol at no less than its bid, otherwise signal=null. Never short, invent symbols/prices, or use future data. Return the required JSON fields.").await
    }
    async fn lead(
        &self,
        system: &str,
        opinions: &[AgentOpinion],
        model: &str,
        tokens: u32,
    ) -> Result<AgentOpinion> {
        self.complete(model, system, &serde_json::to_string(opinions)?, tokens)
            .await
    }
}

#[async_trait::async_trait]
impl crate::backtest::BacktestPolicy for GeminiOrchestrator {
    async fn decide(
        &self,
        snapshot: &MarketSnapshot,
        _position: Option<&crate::backtest::BacktestPosition>,
    ) -> Result<Option<crate::types::Signal>> {
        Ok(self.analyze_backtest(snapshot).await?.signal)
    }
}

pub fn require_complete_cycle(opinions: &[AgentOpinion], expected: usize) -> Result<()> {
    if opinions.len() != expected {
        bail!(
            "incomplete worker cycle: expected {expected}, got {}",
            opinions.len()
        )
    }
    Ok(())
}

fn partition(snapshot: &MarketSnapshot) -> Vec<String> {
    let market = serde_json::to_string(snapshot).unwrap_or_default();
    let lenses = [
        "price and spread",
        "short-term momentum",
        "volatility and range",
        "volume and liquidity",
        "support and resistance",
        "trend context",
        "risk and invalidation",
        "data quality",
        "event sensitivity",
        "counter-hypothesis",
    ];
    lenses.iter().enumerate().map(|(i, lens)| format!("objective=common NIFTY assessment; analytical lens={lens}; evidence partition {} of 10: {market}",i+1)).collect()
}

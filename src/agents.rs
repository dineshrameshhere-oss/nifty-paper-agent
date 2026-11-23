use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::{sync::{Mutex, Semaphore}, time::Instant};
use crate::{config::Groq, types::{AgentOpinion, MarketSnapshot}};

#[derive(Clone)]
pub struct GroqOrchestrator {
    client: reqwest::Client,
    config: Groq,
    key: String,
    gate: Arc<Semaphore>,
    last_request: Arc<Mutex<Option<Instant>>>,
}
#[derive(Serialize)]
struct Request<'a> { model: &'a str, messages: [Message<'a>; 2], temperature: f32, max_tokens: u32, response_format: Format }
#[derive(Serialize)]
struct Message<'a> { role: &'a str, content: &'a str }
#[derive(Serialize)]
struct Format { #[serde(rename = "type")] kind: &'static str }
#[derive(Deserialize)]
struct Response { choices: Vec<Choice> }
#[derive(Deserialize)]
struct Choice { message: Content }
#[derive(Deserialize)]
struct Content { content: String }

impl GroqOrchestrator {
    pub fn new(config: Groq, key: String) -> Self {
        let permits = config.max_concurrent_requests.max(1);
        Self { client: reqwest::Client::new(), config, key, gate: Arc::new(Semaphore::new(permits)), last_request: Arc::new(Mutex::new(None)) }
    }
    async fn complete<T: for<'de> Deserialize<'de>>(&self, model: &str, system: &str, prompt: &str, max_tokens: u32) -> Result<T> {
        let _permit = self.gate.acquire().await?;
        // Simple global spacing limiter: safe under the configured RPM, though it does not burst.
        let min_gap = Duration::from_secs_f64(60.0 / self.config.requests_per_minute.max(1) as f64);
        let mut last = self.last_request.lock().await;
        if let Some(at) = *last { tokio::time::sleep(min_gap.saturating_sub(at.elapsed())).await; }
        *last = Some(Instant::now());
        drop(last);
        let body = Request { model, messages: [Message { role: "system", content: system }, Message { role: "user", content: prompt }], temperature: self.config.temperature, max_tokens, response_format: Format { kind: "json_object" } };
        let response: Response = self.client.post(&self.config.base_url).bearer_auth(&self.key).json(&body).send().await?.error_for_status()?.json().await.context("decode Groq response")?;
        let json = response.choices.first().context("Groq returned no choices")?.message.content.trim();
        serde_json::from_str(json).context("parse structured agent response")
    }
    pub async fn analyze(&self, snapshot: &MarketSnapshot) -> Result<AgentOpinion> {
        if self.config.worker_model.starts_with("SET_") || self.config.subleader_model.starts_with("SET_") || self.config.driver_model.starts_with("SET_") { bail!("configure Groq model IDs in settings.toml") }
        let parts = partition(snapshot);
        let mut jobs = Vec::new();
        for (index, part) in parts.into_iter().enumerate() {
            let this = self.clone();
            let objective = "Assess the NIFTY market evidence for a paper-trading opportunity. Return objective, stance, confidence_bps (0-10000), evidence, and signal=null. Do not invent prices or trade instructions.";
            jobs.push(tokio::spawn(async move {
                this.complete::<AgentOpinion>(&this.config.worker_model, objective, &format!("worker={} partition={}",index+1,part),this.config.max_tokens_worker).await
            }));
        }
        let mut opinions = Vec::with_capacity(10);
        for job in jobs { opinions.push(job.await??); }
        require_complete_cycle(&opinions, 10)?;
        let a = self.lead("subleader A: synthesize workers 1-5 without adding claims", &opinions[..5], &self.config.subleader_model, self.config.max_tokens_subleader).await?;
        let b = self.lead("subleader B: synthesize workers 6-10 without adding claims", &opinions[5..], &self.config.subleader_model, self.config.max_tokens_subleader).await?;
        self.lead("Driver: synthesize both subleaders into one final assessment. Preserve signal=null; this starter has no autonomous signal authorization.", &[a,b], &self.config.driver_model, self.config.max_tokens_driver).await
    }
    async fn lead(&self, system: &str, opinions: &[AgentOpinion], model: &str, tokens: u32) -> Result<AgentOpinion> {
        self.complete(model, system, &serde_json::to_string(opinions)?, tokens).await
    }
}

pub fn require_complete_cycle(opinions: &[AgentOpinion], expected: usize) -> Result<()> {
    if opinions.len() != expected { bail!("incomplete worker cycle: expected {expected}, got {}", opinions.len()) }
    Ok(())
}

fn partition(snapshot: &MarketSnapshot) -> Vec<String> {
    let market = serde_json::to_string(snapshot).unwrap_or_default();
    let lenses = ["price and spread", "short-term momentum", "volatility and range", "volume and liquidity", "support and resistance", "trend context", "risk and invalidation", "data quality", "event sensitivity", "counter-hypothesis"];
    lenses.iter().enumerate().map(|(i, lens)| format!("objective=common NIFTY assessment; analytical lens={lens}; evidence partition {} of 10: {market}",i+1)).collect()
}

#![allow(dead_code)]

use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use nifty_paper_agent::{
    agents::{DeterministicLlmBackend, LlmBackend, TradingAgentsOrchestrator},
    clock::FakeClock,
    config::Settings,
    data::MarketDataFeed,
    journal::Journal,
    paper_runner::PaperRunner,
    types::{MarketSnapshot, PositionContext, Quote, Side, Signal, TraderProposal},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tempfile::NamedTempFile;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    task::JoinHandle,
};
use uuid::Uuid;

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn bin_exe() -> &'static str {
    env!("CARGO_BIN_EXE_nifty-paper-agent")
}

pub fn ist_utc(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> DateTime<Utc> {
    let offset = FixedOffset::east_opt(5 * 3600 + 30 * 60).expect("valid IST offset");
    let date = NaiveDate::from_ymd_opt(y, m, d).expect("valid date");
    let time = NaiveTime::from_hms_opt(hh, mm, ss).expect("valid time");
    let naive = NaiveDateTime::new(date, time);
    offset
        .from_local_datetime(&naive)
        .single()
        .expect("unambiguous IST datetime")
        .with_timezone(&Utc)
}

pub fn load_repo_settings() -> Settings {
    Settings::load(repo_root().join("settings.toml")).expect("load repo settings.toml")
}

pub fn test_settings(rest_base_url: Option<&str>) -> Settings {
    let mut s = load_repo_settings();
    if let Some(url) = rest_base_url {
        s.indstocks.rest_base_url = url.to_string();
    }
    s.indstocks.market_data_enabled = true;
    s.llm.use_offline_deterministic = true;
    s
}

pub fn temp_journal() -> (NamedTempFile, Arc<Journal>) {
    let db = NamedTempFile::new().expect("create temp sqlite file");
    let journal = Arc::new(
        Journal::open(db.path().to_str().expect("utf8 temp path")).expect("open temp journal"),
    );
    (db, journal)
}

pub fn make_runner_deterministic(settings: Settings) -> (NamedTempFile, Arc<Journal>, PaperRunner) {
    let (db, journal) = temp_journal();
    let orch =
        TradingAgentsOrchestrator::deterministic(settings.analysis.clone(), settings.llm.clone())
            .with_journal(journal.clone());
    let runner = PaperRunner::new(settings, orch, journal.clone()).expect("create PaperRunner");
    (db, journal, runner)
}

pub fn make_runner_with_backend(
    settings: Settings,
    backend: Arc<dyn LlmBackend>,
) -> (NamedTempFile, Arc<Journal>, PaperRunner) {
    let (db, journal) = temp_journal();
    let orch = TradingAgentsOrchestrator::new(
        settings.analysis.clone(),
        settings.llm.clone(),
        backend,
    )
    .with_journal(journal.clone());
    let runner = PaperRunner::new(settings, orch, journal.clone()).expect("create PaperRunner");
    (db, journal, runner)
}

pub fn make_quote(symbol: &str, bid: f64, ask: f64, last: f64, now: DateTime<Utc>) -> Quote {
    Quote {
        symbol: symbol.to_string(),
        bid,
        ask,
        last,
        timestamp: now,
    }
}

pub fn make_snapshot(now: DateTime<Utc>, quotes: Vec<Quote>, context: &str) -> MarketSnapshot {
    MarketSnapshot {
        id: Uuid::new_v4(),
        created_at: now,
        quotes,
        context: context.to_string(),
    }
}

pub fn buy_signal(
    id: &str,
    symbol: &str,
    quantity: u32,
    entry_price: f64,
    stop_loss_price: f64,
    trailing_stop_loss_price: f64,
    target_price: f64,
) -> Signal {
    Signal {
        id: id.to_string(),
        source: "e2e_test".to_string(),
        symbol: symbol.to_string(),
        side: Side::Buy,
        quantity,
        entry_price,
        target_price,
        stop_loss_price,
        trailing_stop_loss_price,
        confidence_bps: 8000,
        rationale: "e2e test buy".to_string(),
        snapshot_id: None,
    }
}

pub fn clock_sleeper(
    clock: FakeClock,
) -> impl FnMut(std::time::Duration) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
{
    move |d: std::time::Duration| {
        let clk = clock.clone();
        Box::pin(async move {
            let delta = Duration::from_std(d).unwrap_or_else(|_| Duration::seconds(1));
            clk.advance(delta);
        })
    }
}

pub fn write_isolated_settings(dir: &Path, settings: &Settings) -> PathBuf {
    let path = dir.join("settings.toml");
    let raw = toml::to_string_pretty(settings).expect("serialize test settings");
    std::fs::write(&path, raw).expect("write isolated settings.toml");
    path
}

#[derive(Clone, Debug)]
pub struct CapturedHttpRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub target: String,
    pub headers: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct MockHttpResponse {
    pub status_line: String,
    pub body: String,
}

impl MockHttpResponse {
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status_line: "200 OK".to_string(),
            body: body.into(),
        }
    }

    pub fn with_status(status_line: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            status_line: status_line.into(),
            body: body.into(),
        }
    }
}

pub type MockRouteFn =
    Arc<dyn Fn(&CapturedHttpRequest) -> MockHttpResponse + Send + Sync + 'static>;

pub struct MockIndstocksServer {
    base_url: String,
    requests: Arc<Mutex<Vec<CapturedHttpRequest>>>,
    non_get_rejections: Arc<AtomicUsize>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl MockIndstocksServer {
    pub async fn start_with_handler<F>(reject_non_get: bool, handler: F) -> Self
    where
        F: Fn(&CapturedHttpRequest) -> MockHttpResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind MockIndstocksServer");
        let addr = listener.local_addr().expect("local_addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let non_get_rejections = Arc::new(AtomicUsize::new(0));
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let reqs_clone = Arc::clone(&requests);
        let rej_clone = Arc::clone(&non_get_rejections);
        let handler_arc: MockRouteFn = Arc::new(handler);

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((mut stream, _)) = accepted else { break };
                        let reqs = Arc::clone(&reqs_clone);
                        let rej = Arc::clone(&rej_clone);
                        let h = Arc::clone(&handler_arc);
                        tokio::spawn(async move {
                            let mut buf = Vec::with_capacity(4096);
                            let mut tmp = [0u8; 1024];
                            loop {
                                let n = match stream.read(&mut tmp).await {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => n,
                                };
                                buf.extend_from_slice(&tmp[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            let raw = String::from_utf8_lossy(&buf);
                            let mut lines = raw.split("\r\n");
                            let request_line = lines.next().unwrap_or_default();
                            let mut parts = request_line.split_whitespace();
                            let method = parts.next().unwrap_or_default().to_string();
                            let target = parts.next().unwrap_or_default().to_string();
                            let (path, query) = target
                                .split_once('?')
                                .map(|(p, q)| (p.to_string(), q.to_string()))
                                .unwrap_or_else(|| (target.clone(), String::new()));

                            let mut headers = BTreeMap::new();
                            for line in lines {
                                if line.is_empty() {
                                    break;
                                }
                                if let Some((k, v)) = line.split_once(':') {
                                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                                }
                            }

                            let captured = CapturedHttpRequest {
                                method: method.clone(),
                                path,
                                query,
                                target,
                                headers,
                            };
                            reqs.lock().expect("requests lock").push(captured.clone());

                            let resp = if reject_non_get && method != "GET" {
                                rej.fetch_add(1, Ordering::SeqCst);
                                MockHttpResponse::with_status(
                                    "405 Method Not Allowed",
                                    r#"{"status":"error","error_type":"MethodNotAllowedException","message":"Only read-only GET allowed in paper mode"}"#,
                                )
                            } else {
                                h(&captured)
                            };

                            let http_resp = format!(
                                "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                resp.status_line,
                                resp.body.len(),
                                resp.body
                            );
                            let _ = stream.write_all(http_resp.as_bytes()).await;
                            let _ = stream.shutdown().await;
                        });
                    }
                }
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            requests,
            non_get_rejections,
            shutdown_tx: Some(shutdown_tx),
            handle: Some(handle),
        }
    }

    pub async fn start_queued(responses: Vec<MockHttpResponse>) -> Self {
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
        Self::start_with_handler(true, move |_req| {
            queue
                .lock()
                .expect("queue lock")
                .pop_front()
                .unwrap_or_else(|| {
                    MockHttpResponse::with_status(
                        "500 Internal Server Error",
                        r#"{"status":"error","message":"mock response queue exhausted"}"#,
                    )
                })
        })
        .await
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn requests(&self) -> Vec<CapturedHttpRequest> {
        self.requests.lock().expect("requests lock").clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("requests lock").len()
    }

    pub fn non_get_rejections(&self) -> usize {
        self.non_get_rejections.load(Ordering::SeqCst)
    }

    pub fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

impl Drop for MockIndstocksServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

pub fn sample_expiries_json(expiries: &[&str]) -> String {
    json!({
        "status": "success",
        "data": expiries
    })
    .to_string()
}

#[allow(clippy::too_many_arguments)]
pub fn sample_option_chain_json(
    spot: f64,
    expiry: &str,
    strike_key: &str,
    ce_symbol: &str,
    ce_bid: f64,
    ce_ask: f64,
    ce_last: f64,
    pe_symbol: &str,
    pe_bid: f64,
    pe_ask: f64,
    pe_last: f64,
) -> String {
    json!({
        "status": "success",
        "data": {
            "underlying_ltp": spot,
            "expiry": expiry,
            "strikes": {
                strike_key: {
                    "ce": {
                        "security_id": "45108",
                        "trading_symbol": ce_symbol,
                        "last_price": ce_last,
                        "top_bid_price": ce_bid,
                        "top_ask_price": ce_ask,
                        "oi": 150000,
                        "previous_oi": 140000,
                        "volume": 25000,
                        "iv": 13.5,
                        "greeks": { "delta": 0.52, "gamma": 0.002, "theta": -14.2, "vega": 8.4 }
                    },
                    "pe": {
                        "security_id": "45109",
                        "trading_symbol": pe_symbol,
                        "last_price": pe_last,
                        "top_bid_price": pe_bid,
                        "top_ask_price": pe_ask,
                        "oi": 180000,
                        "previous_oi": 170000,
                        "volume": 22000,
                        "iv": 14.1,
                        "greeks": { "delta": -0.48, "gamma": 0.002, "theta": -13.8, "vega": 8.3 }
                    }
                }
            }
        }
    })
    .to_string()
}

pub struct ScriptedFeed {
    calls: AtomicUsize,
    handler: Arc<dyn Fn(usize, DateTime<Utc>) -> Result<MarketSnapshot> + Send + Sync>,
}

impl ScriptedFeed {
    pub fn new<F>(handler: F) -> Self
    where
        F: Fn(usize, DateTime<Utc>) -> Result<MarketSnapshot> + Send + Sync + 'static,
    {
        Self {
            calls: AtomicUsize::new(0),
            handler: Arc::new(handler),
        }
    }

    pub fn from_queue(snapshots: Vec<Result<MarketSnapshot>>) -> Self {
        let queue = Arc::new(Mutex::new(VecDeque::from(snapshots)));
        Self::new(move |_idx, now| {
            queue
                .lock()
                .expect("feed queue lock")
                .pop_front()
                .unwrap_or_else(|| bail!("ScriptedFeed queue exhausted at {now}"))
        })
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl MarketDataFeed for ScriptedFeed {
    async fn fetch_snapshot(&self, now: DateTime<Utc>) -> Result<MarketSnapshot> {
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        (self.handler)(idx, now)
    }
}

pub type TraderScriptFn =
    Arc<dyn Fn(&MarketSnapshot, Option<&PositionContext>, usize) -> TraderProposal + Send + Sync>;

pub struct ScriptedLlmBackend {
    trader_calls: AtomicUsize,
    total_calls: AtomicUsize,
    delay: Option<std::time::Duration>,
    trader_fn: TraderScriptFn,
}

impl ScriptedLlmBackend {
    pub fn new<F>(trader_fn: F) -> Self
    where
        F: Fn(&MarketSnapshot, Option<&PositionContext>, usize) -> TraderProposal
            + Send
            + Sync
            + 'static,
    {
        Self {
            trader_calls: AtomicUsize::new(0),
            total_calls: AtomicUsize::new(0),
            delay: None,
            trader_fn: Arc::new(trader_fn),
        }
    }

    pub fn with_delay(mut self, delay: std::time::Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    pub fn trader_call_count(&self) -> usize {
        self.trader_calls.load(Ordering::SeqCst)
    }

    pub fn total_call_count(&self) -> usize {
        self.total_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmBackend for ScriptedLlmBackend {
    async fn complete_json(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<Value> {
        self.total_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(d) = self.delay {
            tokio::time::sleep(d).await;
        }
        if system.contains("[STAGE:TRADER]") {
            let idx = self.trader_calls.fetch_add(1, Ordering::SeqCst);
            let payload: Value = serde_json::from_str(prompt).unwrap_or_else(|_| json!({}));
            let snapshot: MarketSnapshot = serde_json::from_value(payload["snapshot"].clone())?;
            let position: Option<PositionContext> =
                serde_json::from_value(payload["position"].clone()).ok();
            let proposal = (self.trader_fn)(&snapshot, position.as_ref(), idx);
            return Ok(serde_json::to_value(proposal)?);
        }
        DeterministicLlmBackend
            .complete_json(model, system, prompt, max_tokens)
            .await
    }
}

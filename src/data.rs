use crate::{
    config::Indstocks,
    types::{MarketSnapshot, Quote},
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use reqwest::{header::AUTHORIZATION, Client, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue},
    MaybeTlsStream, WebSocketStream,
};
use tracing::warn;
use uuid::Uuid;

#[async_trait::async_trait]
pub trait MarketDataFeed: Send + Sync {
    async fn fetch_snapshot(&self, now: DateTime<Utc>) -> Result<MarketSnapshot>;
}

#[derive(Clone)]
pub struct IndstocksClient {
    http: Client,
    config: Indstocks,
    token: String,
    expiry_cache: Arc<Mutex<Option<(NaiveDate, String)>>>,
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[derive(Deserialize)]
struct ExpiriesResponse {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    data: Vec<String>,
}

#[derive(Deserialize)]
struct OptionChainEnvelope {
    data: OptionChainData,
}

#[derive(Deserialize)]
struct OptionChainData {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    underlying_ltp: f64,
    #[serde(default)]
    expiry: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    strikes: BTreeMap<String, Option<OptionStrikeEntry>>,
}

#[derive(Deserialize, Default)]
struct OptionStrikeEntry {
    #[serde(default)]
    ce: Option<OptionLeg>,
    #[serde(default)]
    pe: Option<OptionLeg>,
}

#[derive(Deserialize, Default)]
#[allow(dead_code)]
struct OptionGreeks {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    delta: f64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    gamma: f64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    theta: f64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    vega: f64,
}

#[derive(Deserialize, Default)]
#[allow(dead_code)]
struct OptionLeg {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    security_id: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    trading_symbol: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    last_price: f64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    top_bid_price: f64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    top_ask_price: f64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    oi: i64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    previous_oi: i64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    volume: i64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    iv: f64,
    #[serde(default)]
    greeks: Option<OptionGreeks>,
}

impl IndstocksClient {
    fn build_http_client(
        connect_timeout: std::time::Duration,
        request_timeout: std::time::Duration,
    ) -> Client {
        Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()
            .unwrap_or_else(|err| {
                warn!(
                    error = %err,
                    "failed to build timed reqwest Client; falling back to default Client"
                );
                Client::new()
            })
    }

    pub fn new(config: Indstocks, token: String) -> Self {
        Self {
            http: Self::build_http_client(
                std::time::Duration::from_secs(20),
                std::time::Duration::from_secs(30),
            ),
            config,
            token,
            expiry_cache: Arc::new(Mutex::new(None)),
        }
    }

    fn ensure_enabled_and_url(&self, path: &str) -> Result<()> {
        if !self.config.market_data_enabled {
            bail!("INDstocks adapter is disabled (market_data_enabled = false)")
        }
        if self.config.rest_base_url.trim().is_empty()
            || path.trim().is_empty()
            || self.config.rest_base_url.contains("REPLACE_WITH")
            || path.contains("REPLACE_WITH")
        {
            bail!("verify and configure official INDstocks endpoint and schema first")
        }
        Ok(())
    }

    fn validated_token(&self) -> Result<&str> {
        let tok = self.token.trim();
        if tok.is_empty() || tok.contains(['\r', '\n']) || tok.contains("REPLACE_WITH") {
            bail!(
                "INDSTOCKS_ACCESS_TOKEN is missing or invalid; update .env via scripts/update-indstocks-token.sh (Termux/Unix) or scripts/update-indstocks-token.ps1 (Windows)"
            );
        }
        Ok(tok)
    }

    fn build_url(&self, path: &str) -> String {
        let base = self.config.rest_base_url.trim().trim_end_matches('/');
        let p = path.trim();
        if p.starts_with('/') {
            format!("{base}{p}")
        } else {
            format!("{base}/{p}")
        }
    }

    async fn send_and_decode_json(
        &self,
        req: reqwest::RequestBuilder,
        endpoint: &str,
    ) -> Result<Value> {
        let resp = req
            .send()
            .await
            .map_err(|e| {
                // Surface the root cause (timeout / DNS / TLS / refused) verbosely in the log.
                let kind = if e.is_timeout() {
                    "timeout"
                } else if e.is_connect() {
                    "connection-refused/DNS"
                } else if e.is_request() {
                    "bad-request-build"
                } else {
                    "transport"
                };
                warn!(
                    endpoint = %endpoint,
                    kind = %kind,
                    detail = %e,
                    "INDstocks HTTP send failed"
                );
                anyhow::anyhow!("send INDstocks {endpoint} request: [{kind}] {e}")
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let parsed_body: Option<Value> = serde_json::from_str(&body).ok();

        if !status.is_success() {
            return Err(map_http_error(status, endpoint, &body, parsed_body.as_ref()));
        }

        let value: Value = serde_json::from_str(&body)
            .with_context(|| format!("decode INDstocks {endpoint} JSON response"))?;

        let is_error_status = value
            .get("status")
            .and_then(|s| s.as_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("error"));
        let is_success_false = value.get("success").and_then(|b| b.as_bool()) == Some(false);

        if is_error_status || is_success_false {
            let error_type = value.get("error_type").and_then(|s| s.as_str());
            let message = value
                .get("message")
                .and_then(|s| s.as_str())
                .or_else(|| value.get("error").and_then(|s| s.as_str()))
                .unwrap_or(body.trim());
            let debug_info = value.get("debug_info").and_then(|s| s.as_str());
            let mut text = format!("INDstocks {endpoint} returned error payload: {message}");
            if let Some(et) = error_type {
                text.push_str(&format!(" [{et}]"));
            }
            if let Some(dbg) = debug_info {
                text.push_str(&format!(" (debug_info: {dbg})"));
            }
            if error_type == Some("TokenException")
                || message.to_ascii_lowercase().contains("token")
                || message.to_ascii_lowercase().contains("authorization")
            {
                text.push_str(". Refresh INDSTOCKS_ACCESS_TOKEN by running scripts/update-indstocks-token.sh or scripts/update-indstocks-token.ps1.");
            }
            bail!("{text}");
        }

        Ok(value)
    }

    async fn resolve_expiry(&self, now: DateTime<Utc>, token: &str) -> Result<String> {
        if let Some(ovr) = self
            .config
            .expiry_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            NaiveDate::parse_from_str(ovr, "%Y-%m-%d").with_context(|| {
                format!("invalid indstocks.expiry_override '{ovr}'; expected YYYY-MM-DD")
            })?;
            return Ok(ovr.to_string());
        }

        let today_ist = crate::clock::to_ist(now).date_naive();
        let mut cache = self.expiry_cache.lock().await;
        if let Some((cached_date, ref cached_expiry)) = *cache {
            if cached_date == today_ist {
                return Ok(cached_expiry.clone());
            }
        }

        self.ensure_enabled_and_url(&self.config.expiries_path)?;
        let url = self.build_url(&self.config.expiries_path);
        let req = self
            .http
            .get(url)
            .header(AUTHORIZATION, token)
            .query(&[
                ("underlying", self.config.symbol.as_str()),
                ("segment", "DERIVATIVE"),
            ]);
        let value = self.send_and_decode_json(req, "expiries").await?;
        let parsed: ExpiriesResponse =
            serde_json::from_value(value).context("decode INDstocks expiries payload")?;

        let non_empty: Vec<String> = parsed
            .data
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if non_empty.is_empty() {
            bail!(
                "INDstocks expiries returned no contracts for {}",
                self.config.symbol
            );
        }

        let mut dated: Vec<(NaiveDate, String)> = non_empty
            .iter()
            .filter_map(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|d| (d, s.clone())))
            .collect();
        dated.sort_by_key(|(d, _)| *d);

        let chosen = if let Some((_, exp)) = dated.iter().find(|(d, _)| *d >= today_ist) {
            exp.clone()
        } else {
            let fallback = dated
                .last()
                .map(|(_, s)| s.clone())
                .unwrap_or_else(|| non_empty[0].clone());
            warn!(
                today_ist = %today_ist,
                fallback_expiry = %fallback,
                "no INDstocks expiry >= today_ist; falling back to most recent returned expiry"
            );
            fallback
        };

        *cache = Some((today_ist, chosen.clone()));
        Ok(chosen)
    }

    async fn fetch_option_chain_snapshot(&self, now: DateTime<Utc>) -> Result<MarketSnapshot> {
        self.ensure_enabled_and_url(&self.config.option_chain_path)?;
        let token = self.validated_token()?;
        let resolved_expiry = self.resolve_expiry(now, token).await?;

        let url = self.build_url(&self.config.option_chain_path);
        let strike_count = self.config.strike_count.to_string();
        let req = self
            .http
            .get(url)
            .header(AUTHORIZATION, token)
            .query(&[
                ("exchange", self.config.exchange.as_str()),
                ("segment", self.config.segment.as_str()),
                ("underlying-scrip", self.config.underlying_scrip.as_str()),
                ("expiry", resolved_expiry.as_str()),
                ("strike_count", strike_count.as_str()),
            ]);

        let value = self.send_and_decode_json(req, "option-chain").await?;
        let envelope: OptionChainEnvelope =
            serde_json::from_value(value).context("decode INDstocks option-chain payload")?;

        let spot = envelope.data.underlying_ltp;
        if !spot.is_finite() || spot <= 0.0 {
            bail!("INDstocks option-chain returned invalid underlying_ltp: {spot}");
        }

        let chain_expiry = envelope
            .data
            .expiry
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(resolved_expiry.as_str())
            .to_string();

        let mut parsed_strikes: Vec<(f64, String, OptionStrikeEntry)> = envelope
            .data
            .strikes
            .into_iter()
            .filter_map(|(k, v)| {
                let entry = v?;
                let parsed = k.trim().replace(',', "").parse::<f64>().ok()?;
                if parsed.is_finite() {
                    Some((parsed, k, entry))
                } else {
                    None
                }
            })
            .collect();
        parsed_strikes.sort_by(|a, b| a.0.total_cmp(&b.0));

        let mut total_call_oi: i64 = 0;
        let mut total_put_oi: i64 = 0;
        let mut all_ivs: Vec<f64> = Vec::new();
        let mut atm_idx: Option<usize> = None;
        let mut best_dist = f64::INFINITY;
        let mut quotes: Vec<Quote> = Vec::new();

        for (idx, (strike_val, _label, entry)) in parsed_strikes.iter().enumerate() {
            let dist = (*strike_val - spot).abs();
            if dist < best_dist {
                best_dist = dist;
                atm_idx = Some(idx);
            }

            if let Some(ce) = entry.ce.as_ref() {
                total_call_oi = total_call_oi.saturating_add(ce.oi.max(0));
                if ce.iv.is_finite() && ce.iv > 0.0 {
                    all_ivs.push(ce.iv);
                }
                if let Some(q) = leg_to_valid_quote(ce, now) {
                    quotes.push(q);
                }
            }
            if let Some(pe) = entry.pe.as_ref() {
                total_put_oi = total_put_oi.saturating_add(pe.oi.max(0));
                if pe.iv.is_finite() && pe.iv > 0.0 {
                    all_ivs.push(pe.iv);
                }
                if let Some(q) = leg_to_valid_quote(pe, now) {
                    quotes.push(q);
                }
            }
        }

        if quotes.is_empty() {
            bail!(
                "INDstocks option chain returned no valid tradable quotes (bid > 0, ask >= bid, last > 0)"
            );
        }

        let pcr = if total_call_oi > 0 {
            total_put_oi as f64 / total_call_oi as f64
        } else {
            0.0
        };

        let atm_label = atm_idx
            .map(|i| parsed_strikes[i].1.as_str())
            .unwrap_or("N/A");

        let mut atm_iv_samples: Vec<f64> = Vec::new();
        if let Some(i) = atm_idx {
            let (_, _, entry) = &parsed_strikes[i];
            if let Some(ce) = entry.ce.as_ref().filter(|l| l.iv.is_finite() && l.iv > 0.0) {
                atm_iv_samples.push(ce.iv);
            }
            if let Some(pe) = entry.pe.as_ref().filter(|l| l.iv.is_finite() && l.iv > 0.0) {
                atm_iv_samples.push(pe.iv);
            }
        }
        let atm_iv = if !atm_iv_samples.is_empty() {
            atm_iv_samples.iter().sum::<f64>() / atm_iv_samples.len() as f64
        } else if !all_ivs.is_empty() {
            all_ivs.iter().sum::<f64>() / all_ivs.len() as f64
        } else {
            0.0
        };

        let context = format!(
            "{} options spot={:.2} expiry={} atm={} call_oi={} put_oi={} pcr={:.2} atm_iv={:.2}% quotes={}",
            self.config.symbol,
            spot,
            chain_expiry,
            atm_label,
            total_call_oi,
            total_put_oi,
            pcr,
            atm_iv,
            quotes.len()
        );

        Ok(MarketSnapshot {
            id: Uuid::new_v4(),
            created_at: now,
            quotes,
            context,
        })
    }

    pub async fn fetch_quotes(
        &self,
        scrip_codes: &[String],
        now: DateTime<Utc>,
    ) -> Result<Vec<Quote>> {
        self.ensure_enabled_and_url(&self.config.quote_path)?;
        let token = self.validated_token()?;

        if scrip_codes.is_empty() {
            return Ok(Vec::new());
        }

        let mut trimmed_codes: Vec<&str> = Vec::with_capacity(scrip_codes.len());
        for code in scrip_codes {
            let t = code.trim();
            if t.is_empty() {
                bail!("scrip_codes must not contain blank entries");
            }
            trimmed_codes.push(t);
        }

        let joined = trimmed_codes.join(",");
        let url = self.build_url(&self.config.quote_path);
        let req = self
            .http
            .get(url)
            .header(AUTHORIZATION, token)
            .query(&[("scrip-codes", joined.as_str())]);

        let value = self.send_and_decode_json(req, "quotes").await?;
        let data = value
            .get("data")
            .and_then(|v| v.as_object())
            .context("INDstocks quotes payload missing 'data' object")?;

        let mut quotes = Vec::with_capacity(trimmed_codes.len());
        for code in trimmed_codes {
            let entry = data
                .get(code)
                .with_context(|| format!("INDstocks quotes response missing data for '{code}'"))?;
            let live_price = entry
                .get("live_price")
                .and_then(parse_indstocks_f64)
                .filter(|v| *v > 0.0)
                .with_context(|| {
                    format!("INDstocks quotes response missing valid live_price for '{code}'")
                })?;

            let (bid, ask) = extract_bid_ask_from_market_depth(entry, code, live_price);
            let quote = Quote {
                symbol: code.to_string(),
                bid,
                ask,
                last: live_price,
                timestamp: now,
            };
            validate_quote_at(&quote, now)?;
            quotes.push(quote);
        }

        Ok(quotes)
    }

    pub async fn quote(&self) -> Result<Quote> {
        let snap = self.fetch_snapshot(Utc::now()).await?;
        snap.quotes
            .into_iter()
            .next()
            .context("INDstocks option chain returned no quotes")
    }

    pub async fn snapshot(&self) -> Result<MarketSnapshot> {
        self.fetch_snapshot(Utc::now()).await
    }

    pub fn websocket_url(&self) -> &str {
        &self.config.websocket_url
    }

    pub async fn connect_websocket(
        &self,
    ) -> Result<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>> {
        if !self.config.market_data_enabled {
            bail!("INDstocks adapter is disabled (market_data_enabled = false)")
        }
        if self.config.websocket_url.trim().is_empty()
            || self.config.websocket_url.contains("REPLACE_WITH")
        {
            bail!("verify and configure the official INDstocks WebSocket URL first")
        }
        let token = self.validated_token()?;
        let mut req = self.websocket_url().into_client_request()?;
        req.headers_mut()
            .insert("Authorization", HeaderValue::from_str(token)?);
        let (socket, _) = connect_async(req).await?;
        Ok(socket)
    }
}

#[async_trait::async_trait]
impl MarketDataFeed for IndstocksClient {
    async fn fetch_snapshot(&self, now: DateTime<Utc>) -> Result<MarketSnapshot> {
        self.fetch_option_chain_snapshot(now).await
    }
}

fn leg_to_valid_quote(leg: &OptionLeg, now: DateTime<Utc>) -> Option<Quote> {
    let symbol = if !leg.trading_symbol.trim().is_empty() {
        leg.trading_symbol.trim().to_string()
    } else if !leg.security_id.trim().is_empty() {
        format!("NFO_{}", leg.security_id.trim())
    } else {
        return None;
    };

    let quote = Quote {
        symbol,
        bid: leg.top_bid_price,
        ask: leg.top_ask_price,
        last: leg.last_price,
        timestamp: now,
    };
    if validate_quote_at(&quote, now).is_ok() {
        Some(quote)
    } else {
        None
    }
}

fn parse_indstocks_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().filter(|x| x.is_finite()),
        Value::String(s) => s
            .trim()
            .replace(',', "")
            .parse::<f64>()
            .ok()
            .filter(|x| x.is_finite()),
        _ => None,
    }
}

fn find_depth_array<'a>(entry: &'a Value, code: &str) -> Option<&'a Vec<Value>> {
    let md = entry.get("market_depth")?.as_object()?;
    if let Some(depth) = md.get("depth").and_then(|d| d.as_array()) {
        return Some(depth);
    }
    if let Some(depth) = md
        .get(code)
        .and_then(|inner| inner.get("depth"))
        .and_then(|d| d.as_array())
    {
        return Some(depth);
    }
    for val in md.values() {
        if let Some(depth) = val.get("depth").and_then(|d| d.as_array()) {
            return Some(depth);
        }
    }
    None
}

fn extract_bid_ask_from_market_depth(entry: &Value, code: &str, live_price: f64) -> (f64, f64) {
    let Some(depth) = find_depth_array(entry, code) else {
        return (live_price, live_price);
    };
    let Some(top) = depth.first() else {
        return (live_price, live_price);
    };
    let buy_px = top
        .get("buy")
        .and_then(|b| b.get("price"))
        .and_then(parse_indstocks_f64)
        .filter(|v| *v > 0.0);
    let sell_px = top
        .get("sell")
        .and_then(|s| s.get("price"))
        .and_then(parse_indstocks_f64)
        .filter(|v| *v > 0.0);

    match (buy_px, sell_px) {
        (Some(b), Some(s)) => (b, s),
        (Some(b), None) => (b, live_price.max(b)),
        (None, Some(s)) => (live_price.min(s), s),
        (None, None) => (live_price, live_price),
    }
}

fn map_http_error(
    status: StatusCode,
    endpoint: &str,
    raw_body: &str,
    parsed: Option<&Value>,
) -> anyhow::Error {
    let msg = parsed
        .and_then(|v| v.get("message"))
        .and_then(|v| v.as_str());
    let error_type = parsed
        .and_then(|v| v.get("error_type"))
        .and_then(|v| v.as_str());
    let debug_info = parsed
        .and_then(|v| v.get("debug_info"))
        .and_then(|v| v.as_str());
    let err_field = parsed.and_then(|v| v.get("error")).and_then(|v| v.as_str());

    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            let et_suffix = error_type
                .map(|et| format!(" [{et}]"))
                .unwrap_or_default();
            let detail = msg.or(err_field).unwrap_or(raw_body.trim());
            anyhow::anyhow!(
                "INDstocks authentication failed (HTTP {}{}): {}. Refresh INDSTOCKS_ACCESS_TOKEN by running scripts/update-indstocks-token.sh or scripts/update-indstocks-token.ps1.",
                status.as_u16(),
                et_suffix,
                detail
            )
        }
        StatusCode::BAD_REQUEST => {
            let detail = msg.or(err_field).unwrap_or(raw_body.trim());
            let mut text = format!("INDstocks bad request (HTTP 400) on {endpoint}: {detail}");
            if let Some(dbg) = debug_info {
                text.push_str(&format!(" (debug_info: {dbg})"));
            }
            if detail.to_ascii_lowercase().contains("authorization") {
                text.push_str(". Refresh INDSTOCKS_ACCESS_TOKEN by running scripts/update-indstocks-token.sh or scripts/update-indstocks-token.ps1.");
            }
            anyhow::anyhow!(text)
        }
        StatusCode::TOO_MANY_REQUESTS => {
            let detail = err_field.or(msg).unwrap_or(raw_body.trim());
            anyhow::anyhow!(
                "INDstocks rate limit exceeded (HTTP 429) on {endpoint}: {detail}; slow down polling interval"
            )
        }
        _ => anyhow::anyhow!(
            "INDstocks {endpoint} returned HTTP {}: {}",
            status.as_u16(),
            raw_body.trim()
        ),
    }
}

pub fn validate_quote_at(q: &Quote, now: DateTime<Utc>) -> Result<()> {
    if q.symbol.trim().is_empty()
        || !q.bid.is_finite()
        || !q.ask.is_finite()
        || !q.last.is_finite()
        || q.bid <= 0.0
        || q.ask < q.bid
        || q.last <= 0.0
    {
        bail!("invalid quote fields")
    }
    if q.timestamp > now + Duration::seconds(5) {
        bail!("quote timestamp is in the future")
    }
    Ok(())
}

pub fn validate_quote(q: &Quote) -> Result<()> {
    validate_quote_at(q, Utc::now())
}

pub fn ensure_fresh_at(q: &Quote, max_age_seconds: i64, now: DateTime<Utc>) -> Result<()> {
    validate_quote_at(q, now)?;
    if now - q.timestamp > Duration::seconds(max_age_seconds) {
        bail!("stale market data")
    }
    Ok(())
}

pub fn ensure_fresh(q: &Quote, max_age_seconds: i64) -> Result<()> {
    ensure_fresh_at(q, max_age_seconds, Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::VecDeque;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[derive(Clone, Debug)]
    struct CapturedRequest {
        target: String,
        auth_header: Option<String>,
    }

    #[derive(Clone, Debug)]
    struct MockHttpResponse {
        status_line: &'static str,
        body: String,
    }

    impl MockHttpResponse {
        fn ok(body: impl Into<String>) -> Self {
            Self {
                status_line: "200 OK",
                body: body.into(),
            }
        }

        fn with_status(status_line: &'static str, body: impl Into<String>) -> Self {
            Self {
                status_line,
                body: body.into(),
            }
        }
    }

    async fn spawn_mock_server(
        responses: Vec<MockHttpResponse>,
    ) -> (
        String,
        Arc<std::sync::Mutex<Vec<CapturedRequest>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let queue = Arc::new(std::sync::Mutex::new(VecDeque::from(responses)));
        let captured_clone = Arc::clone(&captured);
        let queue_clone = Arc::clone(&queue);

        let handle = tokio::spawn(async move {
            loop {
                if queue_clone.lock().unwrap().is_empty() {
                    break;
                }
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };

                let mut buf = Vec::with_capacity(2048);
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

                let raw_req = String::from_utf8_lossy(&buf);
                let mut lines = raw_req.split("\r\n");
                let request_line = lines.next().unwrap_or_default();
                let target = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();

                let mut auth_header = None;
                for line in lines {
                    if line.is_empty() {
                        break;
                    }
                    if let Some((name, val)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("authorization") {
                            auth_header = Some(val.trim().to_string());
                        }
                    }
                }

                captured_clone
                    .lock()
                    .unwrap()
                    .push(CapturedRequest { target, auth_header });

                let resp = queue_clone
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| MockHttpResponse::with_status("500 Internal Server Error", "{}"));

                let http_resp = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp.status_line,
                    resp.body.len(),
                    resp.body
                );
                let _ = stream.write_all(http_resp.as_bytes()).await;
                let _ = stream.flush().await;
                let _ = stream.shutdown().await;
            }
        });

        (format!("http://{addr}"), captured, handle)
    }

    fn test_config(base_url: String) -> Indstocks {
        Indstocks {
            rest_base_url: base_url,
            market_data_enabled: true,
            ..Indstocks::default()
        }
    }

    fn sample_option_chain_json(expiry: &str) -> String {
        format!(
            r#"{{
              "status": "success",
              "data": {{
                "underlying_ltp": 24455.25,
                "expiry": "{expiry}",
                "strikes": {{
                  "24500": {{
                    "ce": {{
                      "security_id": "51013",
                      "trading_symbol": "NIFTY 27 NOV 24500 CALL",
                      "last_price": 115.4,
                      "top_bid_price": 115.0,
                      "top_ask_price": 116.0,
                      "oi": 200000,
                      "previous_oi": 180000,
                      "volume": 50000,
                      "iv": 11.2,
                      "greeks": {{"delta": 0.45, "gamma": 0.001, "theta": -12.0, "vega": 8.0}}
                    }},
                    "pe": {{
                      "security_id": "51014",
                      "trading_symbol": "NIFTY 27 NOV 24500 PUT",
                      "last_price": 160.2,
                      "top_bid_price": 0.0,
                      "top_ask_price": 161.0,
                      "oi": 300000,
                      "previous_oi": 280000,
                      "volume": 40000,
                      "iv": 12.4
                    }}
                  }},
                  "24450": {{
                    "ce": {{
                      "security_id": "51011",
                      "trading_symbol": "NIFTY 27 NOV 24450 CALL",
                      "last_price": 142.5,
                      "top_bid_price": 142.0,
                      "top_ask_price": 143.0,
                      "oi": 150000,
                      "previous_oi": 140000,
                      "volume": 75000,
                      "iv": 10.5
                    }},
                    "pe": {{
                      "security_id": "51012",
                      "trading_symbol": "NIFTY 27 NOV 24450 PUT",
                      "last_price": 135.0,
                      "top_bid_price": 136.0,
                      "top_ask_price": 135.0,
                      "oi": 175000,
                      "previous_oi": 160000,
                      "volume": 65000,
                      "iv": 11.5
                    }}
                  }}
                }}
              }}
            }}"#
        )
    }

    #[tokio::test]
    async fn rejects_empty_and_whitespace_token_before_http_call() {
        let (base_url, captured, _handle) = spawn_mock_server(vec![]).await;
        let now = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();

        for bad_tok in ["", "   ", "bad\ntoken", "REPLACE_WITH_TOKEN"] {
            let client = IndstocksClient::new(test_config(base_url.clone()), bad_tok.into());
            let err = client.fetch_snapshot(now).await.unwrap_err();
            assert!(
                err.to_string().contains("scripts/update-indstocks-token"),
                "unexpected error for token {bad_tok:?}: {err}"
            );
            let err_q = client
                .fetch_quotes(&["NFO_51011".into()], now)
                .await
                .unwrap_err();
            assert!(err_q.to_string().contains("scripts/update-indstocks-token"));
        }

        assert!(captured.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_disabled_adapter_and_placeholder_urls_without_http_call() {
        let (base_url, captured, _handle) = spawn_mock_server(vec![]).await;
        let now = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();

        let mut disabled = test_config(base_url.clone());
        disabled.market_data_enabled = false;
        let c1 = IndstocksClient::new(disabled, "valid-token".into());
        assert!(c1
            .fetch_snapshot(now)
            .await
            .unwrap_err()
            .to_string()
            .contains("disabled"));

        let mut placeholder_host = test_config("https://REPLACE_WITH_HOST".into());
        placeholder_host.market_data_enabled = true;
        let c2 = IndstocksClient::new(placeholder_host, "valid-token".into());
        assert!(c2.fetch_snapshot(now).await.is_err());

        let mut placeholder_path = test_config(base_url);
        placeholder_path.option_chain_path = "/REPLACE_WITH_PATH".into();
        let c3 = IndstocksClient::new(placeholder_path, "valid-token".into());
        assert!(c3.fetch_snapshot(now).await.is_err());

        assert!(captured.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sends_raw_authorization_header_without_bearer_and_parses_option_chain() {
        let expiries_body = r#"{"status":"success","data":["2026-11-26","2026-12-03","2026-12-10"]}"#;
        let chain_body = sample_option_chain_json("2026-12-03");
        let (base_url, captured, handle) = spawn_mock_server(vec![
            MockHttpResponse::ok(expiries_body),
            MockHttpResponse::ok(chain_body),
        ])
        .await;

        let client = IndstocksClient::new(test_config(base_url), "test-token-raw".into());
        let now = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap(); // 09:30 IST
        let snap = client.fetch_snapshot(now).await.unwrap();
        handle.await.unwrap();

        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        for req in reqs.iter() {
            let auth = req.auth_header.as_deref().unwrap();
            assert_eq!(auth, "test-token-raw");
            assert!(!auth.starts_with("Bearer "));
        }
        assert!(reqs[0]
            .target
            .starts_with("/market/instruments/expiries?underlying=NIFTY&segment=DERIVATIVE"));
        assert!(reqs[1].target.contains("/market/option-chain?"));
        assert!(reqs[1].target.contains("exchange=NSE"));
        assert!(reqs[1].target.contains("segment=INDEX"));
        assert!(reqs[1].target.contains("underlying-scrip=40000001"));
        assert!(reqs[1].target.contains("expiry=2026-12-03"));
        assert!(reqs[1].target.contains("strike_count=10"));

        // 24450 PE has ask < bid (crossed) and 24500 PE has bid == 0.0 -> both filtered out.
        // Remaining valid quotes sorted by strike: 24450 CE then 24500 CE.
        assert_eq!(snap.quotes.len(), 2);
        assert_eq!(snap.quotes[0].symbol, "NIFTY 27 NOV 24450 CALL");
        assert_eq!(snap.quotes[0].bid, 142.0);
        assert_eq!(snap.quotes[0].ask, 143.0);
        assert_eq!(snap.quotes[0].last, 142.5);
        assert_eq!(snap.quotes[1].symbol, "NIFTY 27 NOV 24500 CALL");

        // Rich context check: spot=24455.25, expiry=2026-12-03, atm=24450, call_oi=350000, put_oi=475000, pcr=1.36, atm_iv=11.00%
        assert!(snap.context.contains("spot=24455.25"));
        assert!(snap.context.contains("expiry=2026-12-03"));
        assert!(snap.context.contains("atm=24450"));
        assert!(snap.context.contains("call_oi=350000"));
        assert!(snap.context.contains("put_oi=475000"));
        assert!(snap.context.contains("pcr=1.36"));
        assert!(snap.context.contains("atm_iv=11.00%"));
    }

    #[tokio::test]
    async fn caches_discovered_expiry_per_ist_trading_day_and_rolls_at_ist_midnight() {
        let exp_day1 = r#"{"status":"success","data":["2026-12-03"]}"#;
        let exp_day2 = r#"{"status":"success","data":["2026-12-03","2026-12-10"]}"#;
        let chain = sample_option_chain_json("2026-12-03");
        let (base_url, captured, handle) = spawn_mock_server(vec![
            MockHttpResponse::ok(exp_day1),
            MockHttpResponse::ok(chain.clone()),
            MockHttpResponse::ok(chain.clone()),
            MockHttpResponse::ok(exp_day2),
            MockHttpResponse::ok(chain),
        ])
        .await;

        let client = IndstocksClient::new(test_config(base_url), "tok".into());
        // 2026-11-27 09:30 IST
        let t1 = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();
        // 2026-11-27 23:59 IST
        let t2 = Utc.with_ymd_and_hms(2026, 11, 27, 18, 29, 0).unwrap();
        // 2026-11-28 00:01 IST (rolled over IST date)
        let t3 = Utc.with_ymd_and_hms(2026, 11, 27, 18, 31, 0).unwrap();

        client.fetch_snapshot(t1).await.unwrap();
        client.fetch_snapshot(t2).await.unwrap();
        client.fetch_snapshot(t3).await.unwrap();
        handle.await.unwrap();

        let reqs = captured.lock().unwrap();
        let expiry_calls = reqs
            .iter()
            .filter(|r| r.target.starts_with("/market/instruments/expiries"))
            .count();
        assert_eq!(expiry_calls, 2);
        assert_eq!(reqs.len(), 5);
    }

    #[tokio::test]
    async fn expiry_override_skips_expiries_discovery_endpoint() {
        let chain = sample_option_chain_json("2026-12-24");
        let (base_url, captured, handle) =
            spawn_mock_server(vec![MockHttpResponse::ok(chain)]).await;

        let mut cfg = test_config(base_url);
        cfg.expiry_override = Some("2026-12-24".into());
        let client = IndstocksClient::new(cfg, "tok".into());

        let now = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();
        let snap = client.fetch_snapshot(now).await.unwrap();
        handle.await.unwrap();

        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].target.contains("/market/option-chain?"));
        assert!(reqs[0].target.contains("expiry=2026-12-24"));
        assert!(snap.context.contains("expiry=2026-12-24"));
    }

    #[tokio::test]
    async fn option_chain_with_zero_valid_quotes_returns_error() {
        let empty_chain = r#"{
          "status": "success",
          "data": {
            "underlying_ltp": 24450.0,
            "expiry": "2026-12-03",
            "strikes": {
              "24450": {
                "ce": {
                  "security_id": "51011",
                  "trading_symbol": "NIFTY 03 DEC 24450 CALL",
                  "last_price": 0.0,
                  "top_bid_price": 0.0,
                  "top_ask_price": 0.0
                }
              }
            }
          }
        }"#;
        let (base_url, _captured, handle) =
            spawn_mock_server(vec![MockHttpResponse::ok(empty_chain)]).await;

        let mut cfg = test_config(base_url);
        cfg.expiry_override = Some("2026-12-03".into());
        let client = IndstocksClient::new(cfg, "tok".into());
        let now = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();

        let err = client.fetch_snapshot(now).await.unwrap_err();
        handle.await.unwrap();
        assert!(err.to_string().contains("no valid tradable quotes"));
    }

    #[tokio::test]
    async fn fetch_quotes_handles_flat_nested_depth_comma_strings_ltp_and_empty_input() {
        let full_body = r#"{
          "status": "success",
          "data": {
            "NSE_3045": {
              "live_price": 1102.60,
              "market_depth": {
                "depth": [
                  {
                    "buy": {"quantity": "1,281", "price": "1,102.60"},
                    "sell": {"quantity": "950", "price": "1,103.10"}
                  }
                ]
              }
            },
            "NFO_51011": {
              "live_price": "142.50",
              "market_depth": {
                "NFO_51011": {
                  "depth": [
                    {
                      "buy": {"quantity": "1,281", "price": "142.00"},
                      "sell": {"quantity": "1,500", "price": "143.00"}
                    }
                  ]
                }
              }
            }
          }
        }"#;
        let ltp_body = r#"{
          "status": "success",
          "data": {
            "NFO_51011": { "live_price": 144.25 }
          }
        }"#;

        let (base_url, captured, handle) = spawn_mock_server(vec![
            MockHttpResponse::ok(full_body),
            MockHttpResponse::ok(ltp_body),
        ])
        .await;

        let client = IndstocksClient::new(test_config(base_url), "tok-quotes".into());
        let now = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();

        // Empty input returns Ok(vec![]) without making any HTTP request.
        let empty = client.fetch_quotes(&[], now).await.unwrap();
        assert!(empty.is_empty());

        let quotes = client
            .fetch_quotes(&["NSE_3045".into(), "NFO_51011".into()], now)
            .await
            .unwrap();
        assert_eq!(quotes.len(), 2);
        assert_eq!(quotes[0].symbol, "NSE_3045");
        assert_eq!(quotes[0].bid, 1102.60);
        assert_eq!(quotes[0].ask, 1103.10);
        assert_eq!(quotes[0].last, 1102.60);
        assert_eq!(quotes[1].symbol, "NFO_51011");
        assert_eq!(quotes[1].bid, 142.0);
        assert_eq!(quotes[1].ask, 143.0);
        assert_eq!(quotes[1].last, 142.5);

        let ltp_quotes = client.fetch_quotes(&["NFO_51011".into()], now).await.unwrap();
        assert_eq!(ltp_quotes.len(), 1);
        assert_eq!(ltp_quotes[0].bid, 144.25);
        assert_eq!(ltp_quotes[0].ask, 144.25);
        assert_eq!(ltp_quotes[0].last, 144.25);

        handle.await.unwrap();
        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert!(reqs[0]
            .target
            .contains("scrip-codes=NSE_3045%2CNFO_51011"));
        assert_eq!(reqs[0].auth_header.as_deref(), Some("tok-quotes"));
    }

    #[tokio::test]
    async fn maps_401_403_400_429_and_malformed_json_explicitly() {
        let r401 = MockHttpResponse::with_status(
            "401 Unauthorized",
            r#"{"status":"error","message":"Unable to validate user token","error_type":"TokenException"}"#,
        );
        let r403 = MockHttpResponse::with_status(
            "403 Forbidden",
            r#"{"message":"The provided access_token is either incorrect, expired, or has been revoked.","success":false}"#,
        );
        let r400 = MockHttpResponse::with_status(
            "400 Bad Request",
            r#"{"message":"Bad Request","debug_info":"Invalid exchange, segment, underlying-scrip or expiry passed"}"#,
        );
        let r429 = MockHttpResponse::with_status(
            "429 Too Many Requests",
            r#"{"error":"Rate limit exceeded","success":false}"#,
        );
        let r_malformed = MockHttpResponse::ok("{not valid json");

        let (base_url, _captured, handle) =
            spawn_mock_server(vec![r401, r403, r400, r429, r_malformed]).await;

        let mut cfg = test_config(base_url);
        cfg.expiry_override = Some("2026-12-03".into());
        let client = IndstocksClient::new(cfg, "tok".into());
        let now = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();

        let e401 = client.fetch_snapshot(now).await.unwrap_err().to_string();
        assert!(e401.contains("401"));
        assert!(e401.contains("TokenException"));
        assert!(e401.contains("scripts/update-indstocks-token.sh"));

        let e403 = client.fetch_snapshot(now).await.unwrap_err().to_string();
        assert!(e403.contains("403"));
        assert!(e403.contains("scripts/update-indstocks-token.ps1"));

        let e400 = client.fetch_snapshot(now).await.unwrap_err().to_string();
        assert!(e400.contains("400"));
        assert!(e400.contains("Invalid exchange, segment, underlying-scrip or expiry passed"));

        let e429 = client.fetch_snapshot(now).await.unwrap_err().to_string();
        assert!(e429.contains("429"));
        assert!(e429.contains("Rate limit exceeded"));

        let e_malformed = client.fetch_snapshot(now).await.unwrap_err().to_string();
        assert!(e_malformed.contains("decode INDstocks option-chain JSON response"));

        handle.await.unwrap();
    }

    #[test]
    fn validate_quote_at_and_ensure_fresh_at_are_deterministic() {
        let now = Utc.with_ymd_and_hms(2026, 11, 27, 5, 0, 0).unwrap();
        let valid = Quote {
            symbol: "NIFTY 03 DEC 24450 CALL".into(),
            bid: 100.0,
            ask: 101.0,
            last: 100.5,
            timestamp: now - Duration::seconds(5),
        };
        assert!(validate_quote_at(&valid, now).is_ok());
        assert!(ensure_fresh_at(&valid, 5, now).is_ok());
        assert!(ensure_fresh_at(&valid, 4, now).is_err());

        let future = Quote {
            timestamp: now + Duration::seconds(6),
            ..valid.clone()
        };
        assert!(validate_quote_at(&future, now).is_err());

        let crossed = Quote {
            ask: 99.0,
            ..valid.clone()
        };
        assert!(validate_quote_at(&crossed, now).is_err());

        let zero_bid = Quote {
            bid: 0.0,
            ..valid.clone()
        };
        assert!(validate_quote_at(&zero_bid, now).is_err());

        let blank_sym = Quote {
            symbol: "  ".into(),
            ..valid
        };
        assert!(validate_quote_at(&blank_sym, now).is_err());

        // Backwards-compatible wrappers against Utc::now()
        let fresh_now = Quote {
            symbol: "NIFTY-TEST".into(),
            bid: 50.0,
            ask: 51.0,
            last: 50.5,
            timestamp: Utc::now(),
        };
        assert!(validate_quote(&fresh_now).is_ok());
        assert!(ensure_fresh(&fresh_now, 15).is_ok());
    }

    #[test]
    fn live_order_placement_constant_remains_false() {
        const { assert!(!crate::LIVE_ORDER_PLACEMENT_IMPLEMENTED) };
        assert!(!std::hint::black_box(crate::LIVE_ORDER_PLACEMENT_IMPLEMENTED));
    }

    #[tokio::test]
    async fn option_chain_handles_explicit_null_fields_and_null_strikes_or_ltp() {
        let null_fields_chain = r#"{
          "status": "success",
          "data": {
            "underlying_ltp": 24455.25,
            "expiry": null,
            "strikes": {
              "24450": {
                "ce": {
                  "security_id": "51011",
                  "trading_symbol": null,
                  "last_price": 142.5,
                  "top_bid_price": 142.0,
                  "top_ask_price": 143.0,
                  "oi": null,
                  "previous_oi": null,
                  "volume": null,
                  "iv": null,
                  "greeks": {"delta": null, "gamma": 0.001, "theta": null, "vega": 4.0}
                },
                "pe": {
                  "security_id": "51012",
                  "trading_symbol": "NIFTY 27 NOV 24450 PUT",
                  "last_price": null,
                  "top_bid_price": null,
                  "top_ask_price": null,
                  "oi": null,
                  "iv": null,
                  "greeks": null
                }
              },
              "24500": null
            }
          }
        }"#;
        let null_strikes_chain = r#"{
          "status": "success",
          "data": {
            "underlying_ltp": 24455.25,
            "expiry": "2026-12-03",
            "strikes": null
          }
        }"#;
        let null_ltp_chain = r#"{
          "status": "success",
          "data": {
            "underlying_ltp": null,
            "expiry": "2026-12-03",
            "strikes": {}
          }
        }"#;

        let (base_url, _captured, handle) = spawn_mock_server(vec![
            MockHttpResponse::ok(null_fields_chain),
            MockHttpResponse::ok(null_strikes_chain),
            MockHttpResponse::ok(null_ltp_chain),
        ])
        .await;

        let mut cfg = test_config(base_url);
        cfg.expiry_override = Some("2026-12-03".into());
        let client = IndstocksClient::new(cfg, "tok".into());
        let now = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();

        let snap = client.fetch_snapshot(now).await.unwrap();
        assert_eq!(snap.quotes.len(), 1);
        assert_eq!(snap.quotes[0].symbol, "NFO_51011");
        assert_eq!(snap.quotes[0].bid, 142.0);
        assert_eq!(snap.quotes[0].ask, 143.0);
        assert_eq!(snap.quotes[0].last, 142.5);
        assert!(snap.context.contains("expiry=2026-12-03"));

        let err_strikes = client.fetch_snapshot(now).await.unwrap_err().to_string();
        assert!(
            err_strikes.contains("no valid tradable quotes"),
            "unexpected strikes:null error: {err_strikes}"
        );

        let err_ltp = client.fetch_snapshot(now).await.unwrap_err().to_string();
        assert!(
            err_ltp.contains("invalid underlying_ltp: 0"),
            "unexpected underlying_ltp:null error: {err_ltp}"
        );

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn resolve_expiry_falls_back_to_most_recent_when_all_expiries_are_past() {
        let expiries_body =
            r#"{"status":"success","data":["2026-11-19","2026-11-12","2026-11-26"]}"#;
        let chain_body = sample_option_chain_json("2026-11-26");
        let (base_url, captured, handle) = spawn_mock_server(vec![
            MockHttpResponse::ok(expiries_body),
            MockHttpResponse::ok(chain_body),
        ])
        .await;

        let client = IndstocksClient::new(test_config(base_url), "tok".into());
        let now = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();
        let snap = client.fetch_snapshot(now).await.unwrap();
        handle.await.unwrap();

        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert!(
            reqs[1].target.contains("expiry=2026-11-26"),
            "expected most recent past expiry 2026-11-26, got target={}",
            reqs[1].target
        );
        assert!(snap.context.contains("expiry=2026-11-26"));
    }

    #[tokio::test]
    async fn option_chain_oi_summation_saturates_without_overflow() {
        let max_oi = i64::MAX;
        let chain_body = format!(
            r#"{{
              "status": "success",
              "data": {{
                "underlying_ltp": 24450.0,
                "expiry": "2026-12-03",
                "strikes": {{
                  "24450": {{
                    "ce": {{
                      "security_id": "51011",
                      "trading_symbol": "NIFTY 03 DEC 24450 CALL",
                      "last_price": 140.0,
                      "top_bid_price": 139.0,
                      "top_ask_price": 141.0,
                      "oi": {max_oi}
                    }},
                    "pe": {{
                      "security_id": "51012",
                      "trading_symbol": "NIFTY 03 DEC 24450 PUT",
                      "last_price": 135.0,
                      "top_bid_price": 134.0,
                      "top_ask_price": 136.0,
                      "oi": {max_oi}
                    }}
                  }},
                  "24500": {{
                    "ce": {{
                      "security_id": "51013",
                      "trading_symbol": "NIFTY 03 DEC 24500 CALL",
                      "last_price": 115.0,
                      "top_bid_price": 114.0,
                      "top_ask_price": 116.0,
                      "oi": {max_oi}
                    }},
                    "pe": {{
                      "security_id": "51014",
                      "trading_symbol": "NIFTY 03 DEC 24500 PUT",
                      "last_price": 160.0,
                      "top_bid_price": 159.0,
                      "top_ask_price": 161.0,
                      "oi": {max_oi}
                    }}
                  }}
                }}
              }}
            }}"#
        );

        let (base_url, _captured, handle) =
            spawn_mock_server(vec![MockHttpResponse::ok(chain_body)]).await;

        let mut cfg = test_config(base_url);
        cfg.expiry_override = Some("2026-12-03".into());
        let client = IndstocksClient::new(cfg, "tok".into());
        let now = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();

        let snap = client.fetch_snapshot(now).await.unwrap();
        handle.await.unwrap();
        assert!(snap.context.contains(&format!("call_oi={max_oi}")));
        assert!(snap.context.contains(&format!("put_oi={max_oi}")));
    }

    #[tokio::test]
    async fn http_client_timeout_aborts_hung_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();

        let server_handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf).await;
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                    _ = &mut done_rx => {}
                }
            }
        });

        let mut cfg = test_config(format!("http://{addr}"));
        cfg.expiry_override = Some("2026-12-03".into());

        // Also exercise production constructor path (10s connect / 15s request timeout).
        let _prod_client = IndstocksClient::new(cfg.clone(), "tok".into());

        let timed_client = IndstocksClient {
            http: IndstocksClient::build_http_client(
                std::time::Duration::from_millis(100),
                std::time::Duration::from_millis(150),
            ),
            config: cfg,
            token: "tok".into(),
            expiry_cache: Arc::new(Mutex::new(None)),
        };

        let now = Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap();
        let started = std::time::Instant::now();
        let err = timed_client.fetch_snapshot(now).await.unwrap_err();
        let elapsed = started.elapsed();

        let _ = done_tx.send(());
        server_handle.await.unwrap();

        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "timeout took too long: {elapsed:?}"
        );
        assert!(
            err.chain()
                .filter_map(|e| e.downcast_ref::<reqwest::Error>())
                .any(|e| e.is_timeout()),
            "expected reqwest timeout error, got: {err:#}"
        );
    }
}

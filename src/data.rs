use crate::{
    config::Indstocks,
    types::{MarketSnapshot, Quote},
};
use anyhow::{bail, Context, Result};
use chrono::{Duration, Utc};
use reqwest::Client;
use serde::Deserialize;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

#[derive(Clone)]
pub struct IndstocksClient {
    http: Client,
    config: Indstocks,
    token: String,
}
#[derive(Deserialize)]
struct QuoteResponse {
    symbol: String,
    bid: f64,
    ask: f64,
    last: f64,
    timestamp: chrono::DateTime<Utc>,
}

impl IndstocksClient {
    pub fn new(config: Indstocks, token: String) -> Self {
        Self {
            http: Client::new(),
            config,
            token,
        }
    }
    pub async fn quote(&self) -> Result<Quote> {
        if !self.config.market_data_enabled {
            bail!("INDstocks adapter is disabled until endpoint/schema verification is complete")
        }
        if self.config.rest_base_url.contains("REPLACE_WITH")
            || self.config.quote_path.contains("REPLACE_WITH")
        {
            bail!("verify and configure official INDstocks endpoint and schema first")
        }
        let url = format!(
            "{}{}",
            self.config.rest_base_url.trim_end_matches('/'),
            self.config.quote_path
        );
        // TODO: verify documented auth header and query parameters for this API version.
        let response: QuoteResponse = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .query(&[("symbol", &self.config.symbol)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("decode verified quote schema")?;
        let quote = Quote {
            symbol: response.symbol,
            bid: response.bid,
            ask: response.ask,
            last: response.last,
            timestamp: response.timestamp,
        };
        validate_quote(&quote)?;
        Ok(quote)
    }
    pub async fn snapshot(&self) -> Result<MarketSnapshot> {
        let quote = self.quote().await?;
        Ok(MarketSnapshot {
            id: uuid::Uuid::new_v4(),
            created_at: Utc::now(),
            quotes: vec![quote],
            context:
                "single verified quote; add verified five-minute candle/option-chain fields here"
                    .into(),
        })
    }
    pub fn websocket_url(&self) -> &str {
        &self.config.websocket_url
    }
    pub async fn connect_websocket(
        &self,
    ) -> Result<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>> {
        if self.config.websocket_url.contains("REPLACE_WITH") {
            bail!("verify and configure the official INDstocks WebSocket URL first")
        }
        // TODO: confirm provider-specific auth headers and subscription frame before sending anything.
        let (socket, _) = connect_async(self.websocket_url()).await?;
        Ok(socket)
    }
}
pub fn validate_quote(q: &Quote) -> Result<()> {
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
    if q.timestamp > Utc::now() + Duration::seconds(5) {
        bail!("quote timestamp is in the future")
    }
    Ok(())
}
pub fn ensure_fresh(q: &Quote, max_age_seconds: i64) -> Result<()> {
    validate_quote(q)?;
    if Utc::now() - q.timestamp > Duration::seconds(max_age_seconds) {
        bail!("stale market data")
    }
    Ok(())
}

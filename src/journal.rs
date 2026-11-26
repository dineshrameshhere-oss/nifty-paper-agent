use crate::types::{Fill, PipelineTrace, RetrievedMemory, Side, Signal, TradeReflection};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, Row};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "was", "are", "that", "this", "from", "into",
];

pub struct Journal {
    conn: Mutex<Connection>,
}

impl Journal {
    pub fn open(path: &str) -> Result<Self> {
        if path != ":memory:" && !path.starts_with("file:") {
            if let Some(parent) = Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create journal directory {:?}", parent))?;
                }
            }
        }

        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        let init_sql = "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS signals(
                 id TEXT PRIMARY KEY,
                 source TEXT NOT NULL,
                 symbol TEXT NOT NULL,
                 side TEXT NOT NULL,
                 quantity INTEGER NOT NULL,
                 price REAL NOT NULL,
                 rationale TEXT NOT NULL,
                 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             CREATE TABLE IF NOT EXISTS fills(
                 id INTEGER PRIMARY KEY,
                 signal_id TEXT NOT NULL UNIQUE,
                 symbol TEXT NOT NULL,
                 side TEXT NOT NULL,
                 quantity INTEGER NOT NULL,
                 price REAL NOT NULL,
                 fees REAL NOT NULL,
                 filled_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS reflections(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 trade_id TEXT NOT NULL,
                 symbol TEXT NOT NULL,
                 side TEXT NOT NULL,
                 pnl_rupees REAL NOT NULL,
                 return_bps INTEGER NOT NULL,
                 exit_reason TEXT NOT NULL DEFAULT '',
                 regime_tags TEXT NOT NULL,
                 situation_summary TEXT NOT NULL,
                 lesson_bull TEXT NOT NULL,
                 lesson_bear TEXT NOT NULL,
                 lesson_trader TEXT NOT NULL,
                 lesson_risk TEXT NOT NULL,
                 lesson_summary TEXT NOT NULL,
                 created_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_reflections_created_at
                 ON reflections(created_at DESC, id DESC);
             CREATE INDEX IF NOT EXISTS idx_reflections_symbol
                 ON reflections(symbol);
             CREATE TABLE IF NOT EXISTS pipeline_traces(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 snapshot_id TEXT NOT NULL,
                 symbol TEXT,
                 approved INTEGER NOT NULL,
                 trace_json TEXT NOT NULL,
                 created_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_pipeline_traces_snapshot_id
                 ON pipeline_traces(snapshot_id);
             CREATE INDEX IF NOT EXISTS idx_pipeline_traces_symbol
                 ON pipeline_traces(symbol);
             CREATE INDEX IF NOT EXISTS idx_pipeline_traces_created_at
                 ON pipeline_traces(created_at DESC, id DESC);";
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5000);
        let mut backoff_ms = 5u64;
        loop {
            match conn.execute_batch(init_sql) {
                Ok(()) => break,
                Err(err) if is_sqlite_busy(&err) && std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                    backoff_ms = (backoff_ms * 2).min(50);
                }
                Err(err) => return Err(err.into()),
            }
        }

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock_conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("journal mutex poisoned"))
    }

    pub fn record_signal(&self, s: &Signal) -> Result<bool> {
        if !s.entry_price.is_finite() {
            bail!("signal entry_price must be finite");
        }
        let conn = self.lock_conn()?;
        let n = conn.execute(
            "INSERT OR IGNORE INTO signals(id,source,symbol,side,quantity,price,rationale) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                s.id,
                s.source,
                s.symbol,
                format!("{:?}", s.side),
                s.quantity,
                s.entry_price,
                s.rationale
            ],
        )?;
        Ok(n == 1)
    }

    pub fn record_fill(&self, f: &Fill) -> Result<()> {
        if !f.price.is_finite() || !f.fees.is_finite() {
            bail!("fill price and fees must be finite");
        }
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO fills(signal_id,symbol,side,quantity,price,fees,filled_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                f.signal_id,
                f.symbol,
                format!("{:?}", f.side),
                f.quantity,
                f.price,
                f.fees,
                f.timestamp.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn record_reflection(&self, reflection: &TradeReflection) -> Result<i64> {
        if !reflection.pnl_rupees.is_finite() {
            bail!("reflection pnl_rupees must be finite");
        }
        let regime_tags_json = serde_json::to_string(&reflection.regime_tags)?;
        let side_str = side_to_str(reflection.side);
        let created_at = format_utc(reflection.created_at);

        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO reflections(
                 trade_id, symbol, side, pnl_rupees, return_bps, exit_reason,
                 regime_tags, situation_summary, lesson_bull, lesson_bear,
                 lesson_trader, lesson_risk, lesson_summary, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                reflection.trade_id,
                reflection.symbol,
                side_str,
                reflection.pnl_rupees,
                reflection.return_bps,
                reflection.exit_reason,
                regime_tags_json,
                reflection.situation_summary,
                reflection.lesson_bull,
                reflection.lesson_bear,
                reflection.lesson_trader,
                reflection.lesson_risk,
                reflection.lesson_summary,
                created_at,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn recent_reflections(&self, limit: usize) -> Result<Vec<TradeReflection>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let sql_limit = limit.min(i64::MAX as usize) as i64;
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, trade_id, symbol, side, pnl_rupees, return_bps, exit_reason,
                    regime_tags, situation_summary, lesson_bull, lesson_bear,
                    lesson_trader, lesson_risk, lesson_summary, created_at
             FROM reflections
             ORDER BY created_at DESC, id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![sql_limit], row_to_reflection)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn query_reflections(
        &self,
        situation_query: &str,
        regime_tags: &[String],
        symbol: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RetrievedMemory>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let candidates: Vec<TradeReflection> = {
            let conn = self.lock_conn()?;
            let mut stmt = conn.prepare(
                "SELECT id, trade_id, symbol, side, pnl_rupees, return_bps, exit_reason,
                        regime_tags, situation_summary, lesson_bull, lesson_bear,
                        lesson_trader, lesson_risk, lesson_summary, created_at
                 FROM reflections
                 ORDER BY created_at DESC, id DESC",
            )?;
            let rows = stmt.query_map([], row_to_reflection)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };

        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let max_ts = candidates
            .iter()
            .map(|r| r.created_at)
            .max()
            .unwrap_or_else(Utc::now);

        let query_tags = normalize_tags(regime_tags);
        let query_tokens = tokenize(situation_query);
        let norm_symbol = symbol.map(str::trim).filter(|s| !s.is_empty());

        let mut scored: Vec<RetrievedMemory> = candidates
            .into_iter()
            .map(|r| {
                let ref_tags = normalize_tags(&r.regime_tags);
                let matched_tags: Vec<String> =
                    query_tags.intersection(&ref_tags).cloned().collect();
                let tag_score = if !query_tags.is_empty() && !ref_tags.is_empty() {
                    let union_len = query_tags.union(&ref_tags).count() as f64;
                    matched_tags.len() as f64 / union_len
                } else {
                    0.0
                };

                let lexical_score = if !query_tokens.is_empty() {
                    let mut ref_tokens = BTreeSet::new();
                    for text in [
                        r.situation_summary.as_str(),
                        r.lesson_summary.as_str(),
                        r.lesson_trader.as_str(),
                        r.lesson_risk.as_str(),
                        r.lesson_bull.as_str(),
                        r.lesson_bear.as_str(),
                        r.symbol.as_str(),
                    ] {
                        ref_tokens.extend(tokenize(text));
                    }
                    if ref_tokens.is_empty() {
                        0.0
                    } else {
                        let overlap = query_tokens.intersection(&ref_tokens).count() as f64;
                        let coverage = overlap / query_tokens.len() as f64;
                        let jaccard = overlap / query_tokens.union(&ref_tokens).count() as f64;
                        0.7 * coverage + 0.3 * jaccard
                    }
                } else {
                    0.0
                };

                let sym_score = norm_symbol
                    .map(|q| symbol_match_score(&r.symbol, q))
                    .unwrap_or(0.0);

                let age_days =
                    (max_ts.signed_duration_since(r.created_at).num_seconds().max(0) as f64)
                        / 86_400.0;
                let recency_score = 1.0 / (1.0 + age_days / 14.0);

                let relevance_score =
                    0.40 * tag_score + 0.30 * lexical_score + 0.15 * sym_score + 0.15 * recency_score;

                RetrievedMemory {
                    reflection: r,
                    relevance_score,
                    matched_tags,
                }
            })
            .collect();

        scored.sort_by(|a, b| {
            b.relevance_score
                .total_cmp(&a.relevance_score)
                .then_with(|| b.reflection.created_at.cmp(&a.reflection.created_at))
                .then_with(|| b.reflection.id.unwrap_or(0).cmp(&a.reflection.id.unwrap_or(0)))
        });
        scored.truncate(limit);
        Ok(scored)
    }

    pub fn record_pipeline_trace(&self, trace: &PipelineTrace) -> Result<()> {
        let snapshot_id = trace.snapshot_id.to_string();
        let symbol = trace
            .final_signal
            .as_ref()
            .or(trace.risk_verdict.final_signal.as_ref())
            .or(trace.trader_proposal.signal.as_ref())
            .map(|s| s.symbol.as_str());
        let approved: i64 = if trace.risk_verdict.approved { 1 } else { 0 };
        let trace_json = serde_json::to_string(trace)?;
        let created_at = format_utc(trace.created_at);

        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO pipeline_traces(snapshot_id, symbol, approved, trace_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![snapshot_id, symbol, approved, trace_json, created_at],
        )?;
        Ok(())
    }

    pub fn signal_count(&self) -> Result<usize> {
        self.table_count("signals")
    }

    pub fn fill_count(&self) -> Result<usize> {
        self.table_count("fills")
    }

    pub fn reflection_count(&self) -> Result<usize> {
        self.table_count("reflections")
    }

    fn table_count(&self, table: &'static str) -> Result<usize> {
        let conn = self.lock_conn()?;
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let n: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
        Ok(usize::try_from(n)?)
    }
}

fn is_sqlite_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error(),
        Some(ffi_err)
            if matches!(
                ffi_err.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn row_to_reflection(row: &Row<'_>) -> rusqlite::Result<TradeReflection> {
    let id: i64 = row.get(0)?;
    let trade_id: String = row.get(1)?;
    let symbol: String = row.get(2)?;
    let side_raw: String = row.get(3)?;
    let pnl_rupees: f64 = row.get(4)?;
    let return_bps: i32 = row.get(5)?;
    let exit_reason: String = row.get(6)?;
    let regime_tags_json: String = row.get(7)?;
    let situation_summary: String = row.get(8)?;
    let lesson_bull: String = row.get(9)?;
    let lesson_bear: String = row.get(10)?;
    let lesson_trader: String = row.get(11)?;
    let lesson_risk: String = row.get(12)?;
    let lesson_summary: String = row.get(13)?;
    let created_at_raw: String = row.get(14)?;

    let side = parse_side(&side_raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let regime_tags: Vec<String> = serde_json::from_str(&regime_tags_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let created_at = parse_utc(&created_at_raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(14, rusqlite::types::Type::Text, Box::new(e))
    })?;

    Ok(TradeReflection {
        id: Some(id),
        trade_id,
        symbol,
        side,
        pnl_rupees,
        return_bps,
        exit_reason,
        situation_summary,
        regime_tags,
        lesson_summary,
        lesson_bull,
        lesson_bear,
        lesson_trader,
        lesson_risk,
        created_at,
    })
}

fn format_utc(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_utc(raw: &str) -> std::result::Result<DateTime<Utc>, chrono::ParseError> {
    DateTime::parse_from_rfc3339(raw).map(|dt| dt.with_timezone(&Utc))
}

fn side_to_str(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

#[derive(Debug)]
struct InvalidSideError(String);

impl std::fmt::Display for InvalidSideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid side: {}", self.0)
    }
}

impl std::error::Error for InvalidSideError {}

fn parse_side(raw: &str) -> std::result::Result<Side, InvalidSideError> {
    if raw.eq_ignore_ascii_case("buy") {
        Ok(Side::Buy)
    } else if raw.eq_ignore_ascii_case("sell") {
        Ok(Side::Sell)
    } else {
        Err(InvalidSideError(raw.to_string()))
    }
}

fn normalize_tags(tags: &[String]) -> BTreeSet<String> {
    tags.iter()
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

fn tokenize(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .map(|tok| tok.to_ascii_lowercase())
        .filter(|tok| tok.len() >= 2 && !STOPWORDS.contains(&tok.as_str()))
        .collect()
}

fn symbol_underlying(sym: &str) -> &str {
    sym.split('-').next().unwrap_or(sym).trim()
}

fn option_right(sym: &str) -> Option<&'static str> {
    let bytes = sym.trim().as_bytes();
    if bytes.len() >= 3 {
        let tail = &bytes[bytes.len() - 3..];
        if tail.eq_ignore_ascii_case(b"-CE") {
            return Some("CE");
        }
        if tail.eq_ignore_ascii_case(b"-PE") {
            return Some("PE");
        }
    }
    None
}

fn symbol_match_score(candidate_sym: &str, query_sym: &str) -> f64 {
    if candidate_sym.eq_ignore_ascii_case(query_sym) {
        return 1.0;
    }
    let cand_underlying = symbol_underlying(candidate_sym);
    let query_underlying = symbol_underlying(query_sym);
    if !cand_underlying.is_empty() && cand_underlying.eq_ignore_ascii_case(query_underlying) {
        if let (Some(cr), Some(qr)) = (option_right(candidate_sym), option_right(query_sym)) {
            if cr == qr {
                return 0.5;
            }
        }
        return 0.25;
    }
    0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AnalystReport, AnalystRole, InvestDebateState, ResearchDecision, RiskDebateState,
        RiskVerdict, TraderProposal,
    };
    use chrono::Duration;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn sample_signal(id: &str, symbol: &str) -> Signal {
        Signal {
            id: id.into(),
            source: "test".into(),
            symbol: symbol.into(),
            side: Side::Buy,
            quantity: 10,
            entry_price: 120.0,
            target_price: 150.0,
            stop_loss_price: 100.0,
            trailing_stop_loss_price: 110.0,
            confidence_bps: 7000,
            rationale: "momentum breakout".into(),
            snapshot_id: Some(Uuid::new_v4()),
        }
    }

    fn sample_reflection(
        trade_id: &str,
        symbol: &str,
        tags: &[&str],
        situation: &str,
        lesson: &str,
        created_at: DateTime<Utc>,
    ) -> TradeReflection {
        TradeReflection {
            id: None,
            trade_id: trade_id.into(),
            symbol: symbol.into(),
            side: Side::Buy,
            pnl_rupees: 450.0,
            return_bps: 375,
            exit_reason: "target_hit".into(),
            situation_summary: situation.into(),
            regime_tags: tags.iter().map(|t| (*t).to_string()).collect(),
            lesson_summary: lesson.into(),
            lesson_bull: "Respect breakout volume".into(),
            lesson_bear: "Check IV crush near expiry".into(),
            lesson_trader: "Tighten trailing stop above +15%".into(),
            lesson_risk: "Cap position size in elevated IV".into(),
            created_at,
        }
    }

    fn sample_trace(snapshot_id: Uuid, created_at: DateTime<Utc>, approved: bool, signal: Option<Signal>) -> PipelineTrace {
        PipelineTrace {
            snapshot_id,
            created_at,
            retrieved_memories: vec![],
            analyst_reports: vec![AnalystReport {
                role: AnalystRole::Technical,
                stance: "bullish".into(),
                confidence_bps: 7200,
                summary: "Spot above VWAP".into(),
                key_levels: vec!["24500".into()],
                evidence: "EMA crossover".into(),
                regime_tags: vec!["trend_up".into()],
            }],
            invest_debate: InvestDebateState::default(),
            research_decision: ResearchDecision {
                stance: if approved { "bullish".into() } else { "hold".into() },
                confidence_bps: 7000,
                thesis: "Breakout".into(),
                investment_plan: "Buy ATM CE".into(),
                preferred_right: Some("CE".into()),
                key_risks: vec!["IV crush".into()],
            },
            trader_proposal: TraderProposal {
                action: if approved { "buy".into() } else { "hold".into() },
                rationale: "Clean R:R".into(),
                confidence_bps: 7000,
                signal: signal.clone(),
            },
            risk_debate: RiskDebateState::default(),
            risk_verdict: RiskVerdict {
                approved,
                decision: if approved { "approve".into() } else { "reject".into() },
                adjusted_confidence_bps: if approved { 6800 } else { 0 },
                verdict_rationale: "Policy check".into(),
                final_signal: signal.clone(),
            },
            final_signal: signal,
        }
    }

    #[test]
    fn open_creates_parent_dir_enables_wal_and_preserves_legacy_db() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("nested").join("subdir").join("paper.sqlite");
        let db_str = db_path.to_str().unwrap();

        // Create legacy DB with only signals and fills first
        {
            let legacy = Connection::open_with_flags(
                // ensure parent exists for legacy setup
                {
                    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
                    &db_path
                },
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
            )
            .unwrap();
            legacy
                .execute_batch(
                    "CREATE TABLE signals(id TEXT PRIMARY KEY, source TEXT NOT NULL, symbol TEXT NOT NULL, side TEXT NOT NULL, quantity INTEGER NOT NULL, price REAL NOT NULL, rationale TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
                     CREATE TABLE fills(id INTEGER PRIMARY KEY, signal_id TEXT NOT NULL UNIQUE, symbol TEXT NOT NULL, side TEXT NOT NULL, quantity INTEGER NOT NULL, price REAL NOT NULL, fees REAL NOT NULL, filled_at TEXT NOT NULL);
                     INSERT INTO signals(id,source,symbol,side,quantity,price,rationale) VALUES ('legacy-1','legacy','NIFTY-CE','Buy',5,100.0,'old');",
                )
                .unwrap();
        }

        let journal = Journal::open(db_str).unwrap();
        assert_eq!(journal.signal_count().unwrap(), 1);

        // Also verify fresh nested creation when parent dir does not exist
        let nested_fresh = dir.path().join("fresh").join("deep").join("journal.sqlite");
        let fresh_journal = Journal::open(nested_fresh.to_str().unwrap()).unwrap();
        let conn = fresh_journal.lock_conn().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");

        let refl_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='reflections';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let trace_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='pipeline_traces';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(refl_table, 1);
        assert_eq!(trace_table, 1);
    }

    #[test]
    fn counts_and_signal_deduplication_work() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("counts.sqlite");
        let journal = Journal::open(db_path.to_str().unwrap()).unwrap();

        assert_eq!(journal.signal_count().unwrap(), 0);
        assert_eq!(journal.fill_count().unwrap(), 0);
        assert_eq!(journal.reflection_count().unwrap(), 0);

        let sig = sample_signal("sig-1", "NIFTY-2026-11-26-24500-CE");
        assert!(journal.record_signal(&sig).unwrap());
        assert!(!journal.record_signal(&sig).unwrap());
        assert_eq!(journal.signal_count().unwrap(), 1);

        let fill = Fill {
            signal_id: "sig-1".into(),
            symbol: sig.symbol.clone(),
            side: Side::Buy,
            quantity: 10,
            price: 120.5,
            fees: 1.5,
            timestamp: Utc::now(),
        };
        journal.record_fill(&fill).unwrap();
        assert_eq!(journal.fill_count().unwrap(), 1);
        assert!(journal.record_fill(&fill).is_err());

        let refl = sample_reflection(
            "tr-1",
            &sig.symbol,
            &["trend_up"],
            "bullish breakout",
            "hold winners",
            Utc::now(),
        );
        let row_id = journal.record_reflection(&refl).unwrap();
        assert!(row_id > 0);
        assert_eq!(journal.reflection_count().unwrap(), 1);
    }

    #[test]
    fn recent_reflections_orders_deterministically_and_handles_zero_limit() {
        let dir = tempdir().unwrap();
        let journal = Journal::open(dir.path().join("recent.sqlite").to_str().unwrap()).unwrap();

        let t0 = DateTime::parse_from_rfc3339("2026-11-26T09:15:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t1 = t0 + Duration::minutes(15);

        let r_old = sample_reflection("t-old", "NIFTY-CE", &["a"], "old", "old lesson", t0);
        let r_tie1 = sample_reflection(
            "t-tie-1",
            "NIFTY-CE",
            &["b"],
            "tie1",
            "tie1 lesson",
            t1,
        );
        let r_tie2 = sample_reflection(
            "t-tie-2",
            "NIFTY-CE",
            &["c"],
            "tie2",
            "tie2 lesson",
            t1,
        );

        journal.record_reflection(&r_old).unwrap();
        let id1 = journal.record_reflection(&r_tie1).unwrap();
        let id2 = journal.record_reflection(&r_tie2).unwrap();
        assert!(id2 > id1);

        assert!(journal.recent_reflections(0).unwrap().is_empty());

        let top2 = journal.recent_reflections(2).unwrap();
        assert_eq!(top2.len(), 2);
        assert_eq!(top2[0].trade_id, "t-tie-2");
        assert_eq!(top2[0].id, Some(id2));
        assert_eq!(top2[1].trade_id, "t-tie-1");
        assert_eq!(top2[1].id, Some(id1));
    }

    #[test]
    fn query_reflections_handles_empty_db_and_zero_limit() {
        let dir = tempdir().unwrap();
        let journal = Journal::open(dir.path().join("empty.sqlite").to_str().unwrap()).unwrap();

        let empty = journal
            .query_reflections("bullish", &["high_iv".into()], Some("NIFTY"), 5)
            .unwrap();
        assert!(empty.is_empty());

        let r = sample_reflection(
            "t-1",
            "NIFTY-2026-11-26-24500-CE",
            &["high_iv"],
            "bullish breakout",
            "lesson",
            Utc::now(),
        );
        journal.record_reflection(&r).unwrap();

        let zero = journal
            .query_reflections("bullish", &["high_iv".into()], Some("NIFTY"), 0)
            .unwrap();
        assert!(zero.is_empty());
    }

    #[test]
    fn query_reflections_ranks_by_regime_tags_lexical_and_symbol() {
        let dir = tempdir().unwrap();
        let journal = Journal::open(dir.path().join("rank.sqlite").to_str().unwrap()).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-11-26T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        // Best match: matching tags, matching lexical terms, exact symbol
        let best = sample_reflection(
            "tr-best",
            "NIFTY-2026-11-26-24500-CE",
            &["high_iv", "trend_up"],
            "spot breakout above vwap with rising volume",
            "enter momentum call early",
            now,
        );
        // Partial match: same underlying & right (CE), only 1 tag, less lexical overlap
        let partial = sample_reflection(
            "tr-partial",
            "NIFTY-2026-11-19-24200-CE",
            &["trend_up", "low_iv"],
            "range expansion near close",
            "scale slowly",
            now,
        );
        // Unrelated: PE option, different tags & text
        let unrelated = sample_reflection(
            "tr-unrelated",
            "BANKNIFTY-2026-11-26-52000-PE",
            &["mean_reversion", "low_iv"],
            "breakdown below support",
            "avoid chasing puts",
            now,
        );

        journal.record_reflection(&unrelated).unwrap();
        journal.record_reflection(&partial).unwrap();
        journal.record_reflection(&best).unwrap();

        let results = journal
            .query_reflections(
                "breakout above vwap rising volume",
                &["high_iv".into(), "trend_up".into()],
                Some("NIFTY-2026-11-26-24500-CE"),
                3,
            )
            .unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].reflection.trade_id, "tr-best");
        assert_eq!(
            results[0].matched_tags,
            vec!["high_iv".to_string(), "trend_up".to_string()]
        );
        assert_eq!(results[1].reflection.trade_id, "tr-partial");
        assert_eq!(results[2].reflection.trade_id, "tr-unrelated");
        assert!(results[0].relevance_score > results[1].relevance_score);
        assert!(results[1].relevance_score > results[2].relevance_score);

        // Verify symbol bonus breaks ties when tags and text match identically
        let dir2 = tempdir().unwrap();
        let sym_journal =
            Journal::open(dir2.path().join("symtie.sqlite").to_str().unwrap()).unwrap();
        let same_text = "identical breakout setup";
        let sym_pe = sample_reflection(
            "sym-pe",
            "NIFTY-2026-11-26-24500-PE",
            &["trend_up"],
            same_text,
            "same lesson",
            now,
        );
        let sym_ce_other = sample_reflection(
            "sym-ce-other",
            "NIFTY-2026-11-19-24300-CE",
            &["trend_up"],
            same_text,
            "same lesson",
            now,
        );
        let sym_ce_exact = sample_reflection(
            "sym-ce-exact",
            "NIFTY-2026-11-26-24500-CE",
            &["trend_up"],
            same_text,
            "same lesson",
            now,
        );
        sym_journal.record_reflection(&sym_pe).unwrap();
        sym_journal.record_reflection(&sym_ce_other).unwrap();
        sym_journal.record_reflection(&sym_ce_exact).unwrap();

        let sym_ranked = sym_journal
            .query_reflections(
                same_text,
                &["trend_up".into()],
                Some("NIFTY-2026-11-26-24500-CE"),
                3,
            )
            .unwrap();
        assert_eq!(sym_ranked[0].reflection.trade_id, "sym-ce-exact");
        assert_eq!(sym_ranked[1].reflection.trade_id, "sym-ce-other");
        assert_eq!(sym_ranked[2].reflection.trade_id, "sym-pe");

        // Empty query/tags degrades cleanly to recency order
        let empty_q = journal.query_reflections("", &[], None, 3).unwrap();
        assert_eq!(empty_q.len(), 3);
    }

    #[test]
    fn query_reflections_breaks_score_ties_by_recency_and_id() {
        let dir = tempdir().unwrap();
        let journal = Journal::open(dir.path().join("recency.sqlite").to_str().unwrap()).unwrap();

        let t0 = DateTime::parse_from_rfc3339("2026-11-20T09:15:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t1 = t0 + Duration::days(1);

        let older = sample_reflection(
            "older",
            "NIFTY-2026-11-26-24500-CE",
            &["trend_up"],
            "same situation",
            "same lesson",
            t0,
        );
        let newer_1 = sample_reflection(
            "newer-1",
            "NIFTY-2026-11-26-24500-CE",
            &["trend_up"],
            "same situation",
            "same lesson",
            t1,
        );
        let newer_2 = sample_reflection(
            "newer-2",
            "NIFTY-2026-11-26-24500-CE",
            &["trend_up"],
            "same situation",
            "same lesson",
            t1,
        );

        journal.record_reflection(&older).unwrap();
        journal.record_reflection(&newer_1).unwrap();
        journal.record_reflection(&newer_2).unwrap();

        let ranked = journal
            .query_reflections(
                "same situation",
                &["trend_up".into()],
                Some("NIFTY-2026-11-26-24500-CE"),
                3,
            )
            .unwrap();
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0].reflection.trade_id, "newer-2");
        assert_eq!(ranked[1].reflection.trade_id, "newer-1");
        assert_eq!(ranked[2].reflection.trade_id, "older");
        assert!(ranked[0].relevance_score > ranked[2].relevance_score);
    }

    #[test]
    fn record_pipeline_trace_persists_metadata_and_json_round_trip() {
        let dir = tempdir().unwrap();
        let journal = Journal::open(dir.path().join("traces.sqlite").to_str().unwrap()).unwrap();

        let snap1 = Uuid::new_v4();
        let snap2 = Uuid::new_v4();
        let t1 = DateTime::parse_from_rfc3339("2026-11-26T09:20:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t2 = t1 + Duration::minutes(5);

        let sig = sample_signal("sig-trace-1", "NIFTY-2026-11-26-24500-CE");
        let trace_ok = sample_trace(snap1, t1, true, Some(sig.clone()));
        let trace_veto = sample_trace(snap2, t2, false, None);

        journal.record_pipeline_trace(&trace_ok).unwrap();
        journal.record_pipeline_trace(&trace_veto).unwrap();

        let conn = journal.lock_conn().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT snapshot_id, symbol, approved, trace_json, created_at
                 FROM pipeline_traces
                 ORDER BY id ASC",
            )
            .unwrap();
        let rows: Vec<(String, Option<String>, i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, snap1.to_string());
        assert_eq!(rows[0].1.as_deref(), Some("NIFTY-2026-11-26-24500-CE"));
        assert_eq!(rows[0].2, 1);
        let decoded1: PipelineTrace = serde_json::from_str(&rows[0].3).unwrap();
        assert_eq!(decoded1.snapshot_id, snap1);
        assert!(decoded1.risk_verdict.approved);
        assert_eq!(decoded1.final_signal.unwrap().id, "sig-trace-1");

        assert_eq!(rows[1].0, snap2.to_string());
        assert_eq!(rows[1].1, None);
        assert_eq!(rows[1].2, 0);
        let decoded2: PipelineTrace = serde_json::from_str(&rows[1].3).unwrap();
        assert_eq!(decoded2.snapshot_id, snap2);
        assert!(!decoded2.risk_verdict.approved);
        assert!(decoded2.final_signal.is_none());
    }

    #[test]
    fn record_reflection_rejects_non_finite_metrics() {
        let dir = tempdir().unwrap();
        let journal = Journal::open(dir.path().join("nan.sqlite").to_str().unwrap()).unwrap();

        let mut r_nan = sample_reflection("nan", "NIFTY-CE", &[], "s", "l", Utc::now());
        r_nan.pnl_rupees = f64::NAN;
        assert!(journal.record_reflection(&r_nan).is_err());

        let mut r_inf = sample_reflection("inf", "NIFTY-CE", &[], "s", "l", Utc::now());
        r_inf.pnl_rupees = f64::INFINITY;
        assert!(journal.record_reflection(&r_inf).is_err());

        assert_eq!(journal.reflection_count().unwrap(), 0);
    }

    #[test]
    fn query_reflections_handles_multibyte_utf8_symbols_without_panic() {
        let dir = tempdir().unwrap();
        let journal = Journal::open(dir.path().join("utf8.sqlite").to_str().unwrap()).unwrap();
        let now = Utc::now();

        let r1 = sample_reflection(
            "utf8-1",
            "₹CE-24500-CE",
            &["trend_up"],
            "rupee prefix breakout",
            "handle utf8 symbols safely",
            now,
        );
        let r2 = sample_reflection(
            "utf8-2",
            "NIFTY-24500—CE",
            &["trend_up"],
            "em dash before CE",
            "handle em dash safely",
            now,
        );
        let r3 = sample_reflection(
            "utf8-3",
            "NIFTY-24500-CE",
            &["trend_up"],
            "ascii ce",
            "ascii lesson",
            now,
        );
        journal.record_reflection(&r1).unwrap();
        journal.record_reflection(&r2).unwrap();
        journal.record_reflection(&r3).unwrap();

        for query_sym in ["₹CE", "₹CE-24500-CE", "NIFTY-24500—CE", "NIFTY—PE", "A-🚀PE"] {
            let results = journal
                .query_reflections("breakout", &["trend_up".into()], Some(query_sym), 5)
                .unwrap();
            assert_eq!(results.len(), 3);
        }
    }

    #[test]
    fn concurrent_open_and_write_succeeds_without_sqlite_busy() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("concurrent.sqlite");
        let db_str = db_path.to_str().unwrap().to_string();

        let threads = 8;
        let writes_per_thread = 10;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(threads));
        let mut handles = Vec::with_capacity(threads);

        for tid in 0..threads {
            let path = db_str.clone();
            let b = std::sync::Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                b.wait();
                let journal = Journal::open(&path).expect("concurrent Journal::open should succeed");
                for i in 0..writes_per_thread {
                    let refl = sample_reflection(
                        &format!("t{tid}-{i}"),
                        "NIFTY-2026-11-26-24500-CE",
                        &["trend_up"],
                        "concurrent setup",
                        "concurrent lesson",
                        Utc::now(),
                    );
                    journal
                        .record_reflection(&refl)
                        .expect("concurrent record_reflection should succeed");
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let journal = Journal::open(&db_str).unwrap();
        assert_eq!(
            journal.reflection_count().unwrap(),
            threads * writes_per_thread
        );
    }

    #[test]
    fn query_reflections_recalls_exact_match_beyond_500_rows() {
        let dir = tempdir().unwrap();
        let journal = Journal::open(dir.path().join("recall551.sqlite").to_str().unwrap()).unwrap();
        let t0 = DateTime::parse_from_rfc3339("2026-01-01T09:15:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let old_exact = sample_reflection(
            "old-exact",
            "NIFTY-2026-01-01-24500-CE",
            &["gamma_squeeze", "vwap_reclaim"],
            "sharp gamma squeeze reclaim above vwap on expiry",
            "scale into confirmed gamma squeeze",
            t0,
        );
        journal.record_reflection(&old_exact).unwrap();

        for i in 1..=550 {
            let noise = sample_reflection(
                &format!("noise-{i}"),
                "BANKNIFTY-2026-11-26-52000-PE",
                &["range_bound", "low_iv"],
                "quiet sideways consolidation midday",
                "stay flat in chop",
                t0 + Duration::minutes(i),
            );
            journal.record_reflection(&noise).unwrap();
        }

        assert_eq!(journal.reflection_count().unwrap(), 551);

        let results = journal
            .query_reflections(
                "gamma squeeze reclaim above vwap expiry",
                &["gamma_squeeze".into(), "vwap_reclaim".into()],
                Some("NIFTY-2026-01-01-24500-CE"),
                5,
            )
            .unwrap();

        assert_eq!(results.len(), 5);
        assert_eq!(results[0].reflection.trade_id, "old-exact");
    }

    #[test]
    fn record_signal_and_fill_reject_non_finite_floats() {
        let dir = tempdir().unwrap();
        let journal = Journal::open(dir.path().join("sigfill_nan.sqlite").to_str().unwrap()).unwrap();

        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut sig = sample_signal("bad-sig", "NIFTY-2026-11-26-24500-CE");
            sig.entry_price = bad;
            assert!(journal.record_signal(&sig).is_err());

            let fill_bad_price = Fill {
                signal_id: "bad-fill-price".into(),
                symbol: "NIFTY-2026-11-26-24500-CE".into(),
                side: Side::Buy,
                quantity: 10,
                price: bad,
                fees: 1.0,
                timestamp: Utc::now(),
            };
            assert!(journal.record_fill(&fill_bad_price).is_err());

            let fill_bad_fees = Fill {
                signal_id: "bad-fill-fees".into(),
                symbol: "NIFTY-2026-11-26-24500-CE".into(),
                side: Side::Buy,
                quantity: 10,
                price: 100.0,
                fees: bad,
                timestamp: Utc::now(),
            };
            assert!(journal.record_fill(&fill_bad_fees).is_err());
        }

        assert_eq!(journal.signal_count().unwrap(), 0);
        assert_eq!(journal.fill_count().unwrap(), 0);
    }
}

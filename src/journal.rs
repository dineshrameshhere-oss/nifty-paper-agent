use anyhow::Result;
use rusqlite::{params, Connection};
use crate::types::{Fill, Signal};

pub struct Journal { conn: Connection }
impl Journal {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;
          CREATE TABLE IF NOT EXISTS signals(id TEXT PRIMARY KEY, source TEXT NOT NULL, symbol TEXT NOT NULL, side TEXT NOT NULL, quantity INTEGER NOT NULL, price REAL NOT NULL, rationale TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
          CREATE TABLE IF NOT EXISTS fills(id INTEGER PRIMARY KEY, signal_id TEXT NOT NULL UNIQUE, symbol TEXT NOT NULL, side TEXT NOT NULL, quantity INTEGER NOT NULL, price REAL NOT NULL, fees REAL NOT NULL, filled_at TEXT NOT NULL);")?;
        Ok(Self { conn })
    }
    pub fn record_signal(&self, s: &Signal) -> Result<bool> {
        let n = self.conn.execute("INSERT OR IGNORE INTO signals(id,source,symbol,side,quantity,price,rationale) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![s.id,s.source,s.symbol,format!("{:?}",s.side),s.quantity,s.limit_price,s.rationale])?;
        Ok(n == 1)
    }
    pub fn record_fill(&self, f: &Fill) -> Result<()> {
        self.conn.execute("INSERT INTO fills(signal_id,symbol,side,quantity,price,fees,filled_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![f.signal_id,f.symbol,format!("{:?}",f.side),f.quantity,f.price,f.fees,f.timestamp.to_rfc3339()])?;
        Ok(())
    }
}

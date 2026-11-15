//! SQLite storage for probe readings.

use std::collections::HashMap;

use rusqlite::{Connection, params};
use serde_json::Value;

const SCHEMA: &str = "
CREATE TABLE probes (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);
CREATE TABLE readings (
    probe_id INTEGER NOT NULL REFERENCES probes(id),
    ts       TEXT    NOT NULL,  -- receive time, UTC ISO 8601: 2026-10-18T07:17:21.123Z
    mv       INTEGER NOT NULL,  -- bus voltage, millivolts
    ma       INTEGER NOT NULL,  -- current, milliamps; + discharging, - charging
    PRIMARY KEY (probe_id, ts)
) WITHOUT ROWID;
PRAGMA user_version = 1;
";

pub struct Reading {
    pub probe: String,
    pub ts: String,
    pub mv: i64,
    pub ma: i64,
}

/// Current time as fixed-format UTC ISO 8601 with milliseconds.
pub fn utc_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// A JSON value as text, the way the readings and tool arguments name things.
pub fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

fn milli(value: Option<&Value>) -> Option<i64> {
    let x = match value? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        Value::Bool(b) => f64::from(u8::from(*b)),
        _ => return None,
    };
    let scaled = (x * 1000.0).round_ties_even();
    (scaled.is_finite() && scaled.abs() < 9.0e15).then_some(scaled as i64)
}

/// Turn a probe packet into (probe, mv, ma).
///
/// Packets are JSON like {"v": 12.03, "i": -1.25, "p": -15.04}; power is not
/// stored, since it is v * i. Probes that send no "probe" name are named by
/// their sender address.
pub fn parse_reading(data: &[u8], sender: &str) -> Result<(String, i64, i64), String> {
    let malformed = || {
        let shown = &data[..data.len().min(80)];
        format!("malformed reading b'{}'", shown.escape_ascii())
    };
    let reading: Value = serde_json::from_slice(data).map_err(|_| malformed())?;
    let object = reading.as_object().ok_or_else(malformed)?;
    let mv = milli(object.get("v")).ok_or_else(malformed)?;
    let ma = milli(object.get("i")).ok_or_else(malformed)?;
    let probe = match object.get("probe") {
        Some(name) if !is_falsy(name) => value_text(name),
        _ => sender.to_string(),
    };
    Ok((probe, mv, ma))
}

/// Buffers readings in memory and writes them to SQLite in batches.
pub struct Store {
    db: Connection,
    pub pending: Vec<Reading>,
}

impl Store {
    pub fn open(path: &str) -> rusqlite::Result<Store> {
        let db = Connection::open(path)?;
        // After a power loss the database stays consistent; at worst the last
        // batch is missing.
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            db.execute_batch(SCHEMA)?;
        }
        Ok(Store { db, pending: Vec::new() })
    }

    pub fn add(&mut self, probe: String, ts: String, mv: i64, ma: i64) {
        self.pending.push(Reading { probe, ts, mv, ma });
    }

    /// Write buffered readings in one transaction.
    ///
    /// On failure they stay buffered, and the next flush tries again.
    pub fn flush(&mut self) -> rusqlite::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let tx = self.db.transaction()?;
        {
            let mut ids = HashMap::new();
            let mut add_probe = tx.prepare("INSERT OR IGNORE INTO probes (name) VALUES (?)")?;
            let mut probe_id = tx.prepare("SELECT id FROM probes WHERE name = ?")?;
            let mut add_reading =
                tx.prepare("INSERT OR IGNORE INTO readings (probe_id, ts, mv, ma) VALUES (?, ?, ?, ?)")?;
            for r in &self.pending {
                let id = match ids.get(&r.probe) {
                    Some(&id) => id,
                    None => {
                        add_probe.execute([&r.probe])?;
                        let id: i64 = probe_id.query_row([&r.probe], |row| row.get(0))?;
                        ids.insert(r.probe.clone(), id);
                        id
                    }
                };
                add_reading.execute(params![id, r.ts, r.mv, r.ma])?;
            }
        }
        tx.commit()?;
        self.pending.clear();
        Ok(())
    }

    #[cfg(test)]
    pub fn into_connection(self) -> Connection {
        self.db
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_with_name() {
        let r = parse_reading(br#"{"probe": "battery", "v": 12.85, "i": -1.2345}"#, "10.0.0.1");
        assert_eq!(r, Ok(("battery".into(), 12850, -1234)));
    }

    #[test]
    fn reading_without_name_uses_sender() {
        let r = parse_reading(br#"{"v": 12, "i": 1, "probe": ""}"#, "10.0.0.1");
        assert_eq!(r, Ok(("10.0.0.1".into(), 12000, 1000)));
    }

    #[test]
    fn malformed_readings() {
        for data in [&b"{\"v\": 12}"[..], b"not json", b"[1, 2]", b"{\"v\": \"x\", \"i\": 1}"] {
            let e = parse_reading(data, "10.0.0.1").unwrap_err();
            assert!(e.starts_with("malformed reading b'"), "{e}");
        }
    }

    #[test]
    fn flush_stores_and_clears() {
        let mut store = Store::open(":memory:").unwrap();
        store.add("battery".into(), "2026-11-14T08:00:00.000Z".into(), 12000, 1000);
        store.add("battery".into(), "2026-11-14T08:00:01.000Z".into(), 12000, 1000);
        store.add("solar".into(), "2026-11-14T08:00:01.000Z".into(), 13000, -500);
        store.flush().unwrap();
        assert!(store.pending.is_empty());
        let n: i64 = store.db.query_row("SELECT count(*) FROM readings", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 3);
    }
}

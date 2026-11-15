//! Read-only JSON API over the stored readings.
//!
//! Runs in background threads of the collector and opens the database read-only
//! for each request, so it never blocks or alters what the collector writes.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::thread;

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags, named_params};
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::mcp;

const MAX_BUCKETS: i64 = 5000;
const MIN_BUCKET: i64 = 2;
const DEFAULT_RANGE_HOURS: i64 = 24;
// Longer gaps between readings count as this long, so an outage isn't counted
// as current flowing all along.
const MAX_GAP_MS: i64 = 10_000;
const MAX_BODY: usize = 64 * 1024;
const WORKERS: usize = 4;

pub type Params = HashMap<String, String>;

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    Database(rusqlite::Error),
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError::Database(e)
    }
}

type Rows = Result<Vec<Value>, ApiError>;

fn bad(message: impl Into<String>) -> ApiError {
    ApiError::BadRequest(message.into())
}

/// A string the way Python's repr() shows it, as in the API's error texts.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Round to `digits` decimals like Python's round(): from the exact binary value.
fn round(x: f64, digits: usize) -> f64 {
    format!("{x:.digits$}").parse().unwrap_or(x)
}

fn iso(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn bucket_ts(seconds: i64) -> String {
    match DateTime::from_timestamp(seconds, 0) {
        Some(dt) => dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        None => seconds.to_string(),
    }
}

/// An ISO 8601 time; one without an offset is taken as UTC.
fn parse_iso(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    let (local, offset) = split_offset(value)?;
    let naive = parse_naive(local)?;
    let offset = FixedOffset::east_opt(offset)?;
    Some(offset.from_local_datetime(&naive).single()?.with_timezone(&Utc))
}

fn split_offset(value: &str) -> Option<(&str, i32)> {
    if let Some(local) = value.strip_suffix('Z').or_else(|| value.strip_suffix('z')) {
        return Some((local, 0));
    }
    // An offset can only follow the time, which starts after the 10-character date.
    let Some(at) = value.get(10..).and_then(|t| t.rfind(['+', '-'])).map(|i| i + 10) else {
        return Some((value, 0));
    };
    let sign = if value.as_bytes()[at] == b'-' { -1 } else { 1 };
    let digits: String = value[at + 1..].chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    Some((&value[..at], sign * (hours * 3600 + minutes * 60)))
}

fn parse_naive(local: &str) -> Option<NaiveDateTime> {
    if local.len() == 10 {
        return NaiveDate::parse_from_str(local, "%Y-%m-%d").ok()?.and_hms_opt(0, 0, 0);
    }
    let local = local.replacen(' ', "T", 1);
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%dT%H"]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(&local, format).ok())
}

fn parse_time(name: &str, value: &str) -> Result<DateTime<Utc>, ApiError> {
    parse_iso(value).ok_or_else(|| bad(format!("{name} must be an ISO 8601 time, like 2026-11-14T08:00:00Z")))
}

fn time_range(params: &Params) -> Result<(DateTime<Utc>, DateTime<Utc>), ApiError> {
    let to = match params.get("to") {
        Some(v) => parse_time("to", v)?,
        None => Utc::now(),
    };
    let start = match params.get("from") {
        Some(v) => parse_time("from", v)?,
        None => to - Duration::hours(DEFAULT_RANGE_HOURS),
    };
    if start >= to {
        return Err(bad("from must be before to"));
    }
    Ok((start, to))
}

fn bucket_size(params: &Params, default: i64) -> Result<i64, ApiError> {
    let Some(value) = params.get("bucket") else {
        return Ok(default);
    };
    let bucket: i64 = value.trim().parse().map_err(|_| bad("bucket must be a whole number of seconds"))?;
    if bucket < MIN_BUCKET {
        return Err(bad(format!("bucket must be at least {MIN_BUCKET} seconds")));
    }
    Ok(bucket)
}

struct Range {
    probe: i64,
    from: String,
    to: String,
    bucket: i64,
}

/// Common parameters of /series and /energy.
fn range_query(db: &Connection, params: &Params, default_bucket: i64) -> Result<Range, ApiError> {
    let name = params.get("probe").ok_or_else(|| bad("probe is required"))?;
    let (start, to) = time_range(params)?;
    let bucket = bucket_size(params, default_bucket)?;
    let first = start.timestamp().div_euclid(bucket);
    let last = -(-to.timestamp()).div_euclid(bucket);
    if last - first > MAX_BUCKETS {
        return Err(bad(format!(
            "at most {MAX_BUCKETS} buckets per request; use a larger bucket or a shorter range"
        )));
    }
    let probe = db
        .query_row("SELECT id FROM probes WHERE name = ?", [name], |r| r.get(0))
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => ApiError::NotFound(format!("unknown probe {}", py_repr(name))),
            e => ApiError::Database(e),
        })?;
    Ok(Range { probe, from: iso(start), to: iso(to), bucket })
}

pub fn probes(db: &Connection, _params: &Params) -> Rows {
    let mut stmt = db.prepare(
        "SELECT p.name, r.ts, r.mv, r.ma
         FROM probes p
         LEFT JOIN readings r ON r.probe_id = p.id
             AND r.ts = (SELECT max(ts) FROM readings WHERE probe_id = p.id)
         ORDER BY p.name",
    )?;
    let rows = stmt.query_map([], |r| {
        let name: String = r.get(0)?;
        let ts: Option<String> = r.get(1)?;
        let mv: Option<i64> = r.get(2)?;
        let ma: Option<i64> = r.get(3)?;
        Ok(json!({
            "name": name,
            "ts": ts,
            "v": mv.map(|mv| mv as f64 / 1000.0),
            "i": ma.map(|ma| ma as f64 / 1000.0),
            "p": mv.zip(ma).map(|(mv, ma)| round((mv * ma) as f64 / 1e6, 3)),
        }))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn series(db: &Connection, params: &Params) -> Rows {
    let q = range_query(db, params, 60)?;
    let mut stmt = db.prepare(
        "SELECT CAST(strftime('%s', ts) AS INTEGER) / :bucket * :bucket AS t,
                count(*), avg(mv), min(mv), max(mv), avg(ma), min(ma), max(ma), avg(mv * ma)
         FROM readings
         WHERE probe_id = :probe AND ts >= :from AND ts < :to
         GROUP BY t
         ORDER BY t",
    )?;
    let args = named_params! {":probe": q.probe, ":from": q.from, ":to": q.to, ":bucket": q.bucket};
    let rows = stmt.query_map(args, |r| {
        let f = |i| r.get::<_, f64>(i);
        Ok(json!({
            "ts": bucket_ts(r.get(0)?),
            "n": r.get::<_, i64>(1)?,
            "v_avg": round(f(2)? / 1000.0, 3),
            "v_min": f(3)? / 1000.0,
            "v_max": f(4)? / 1000.0,
            "i_avg": round(f(5)? / 1000.0, 3),
            "i_min": f(6)? / 1000.0,
            "i_max": f(7)? / 1000.0,
            "p_avg": round(f(8)? / 1e6, 3),
        }))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Milliseconds since the epoch of a stored `ts` (2026-10-18T07:17:21.123Z).
fn ts_millis(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    let num = |from: usize, to: usize| -> Option<u32> {
        let digits = b.get(from..to)?;
        digits.iter().try_fold(0u32, |n, d| d.is_ascii_digit().then(|| n * 10 + u32::from(d - b'0')))
    };
    if b.len() == 24 && b[23] == b'Z' && b[19] == b'.' {
        let date = NaiveDate::from_ymd_opt(num(0, 4)? as i32, num(5, 7)?, num(8, 10)?)?;
        let time = date.and_hms_milli_opt(num(11, 13)?, num(14, 16)?, num(17, 19)?, num(20, 23)?)?;
        return Some(time.and_utc().timestamp_millis());
    }
    parse_iso(ts).map(|dt| dt.timestamp_millis())
}

#[derive(Default)]
struct Charge {
    out: i128,
    into: i128,
    energy_out: i128,
    energy_in: i128,
}

pub fn energy(db: &Connection, params: &Params) -> Rows {
    let q = range_query(db, params, 3600)?;
    // Each reading counts until the next one, so the window also takes the
    // first reading after the range. The latest reading counts once the next
    // one has arrived.
    let mut stmt = db.prepare(
        "SELECT ts, mv, ma
         FROM readings
         WHERE probe_id = :probe AND ts >= :from
           AND ts <= coalesce(
               (SELECT min(ts) FROM readings WHERE probe_id = :probe AND ts >= :to), :to)
         ORDER BY ts",
    )?;
    let mut rows = stmt.query(named_params! {":probe": q.probe, ":from": q.from, ":to": q.to})?;
    let mut buckets: Vec<(i64, Charge)> = Vec::new();
    // The previous reading inside the range: (ms, mv, ma).
    let mut previous: Option<(i64, i64, i64)> = None;
    while let Some(row) = rows.next()? {
        let ts: String = row.get(0)?;
        let Some(ms) = ts_millis(&ts) else {
            continue;
        };
        if let Some((prev_ms, mv, ma)) = previous {
            let dt = i128::from((ms - prev_ms).min(MAX_GAP_MS));
            if let Some((_, c)) = buckets.last_mut() {
                let (ma, mv) = (i128::from(ma), i128::from(mv));
                if ma > 0 {
                    c.out += ma * dt;
                    c.energy_out += mv * ma * dt;
                } else if ma < 0 {
                    c.into -= ma * dt;
                    c.energy_in -= mv * ma * dt;
                }
            }
        }
        if ts.as_str() >= q.to.as_str() {
            break;
        }
        let t = ms.div_euclid(1000) / q.bucket * q.bucket;
        if buckets.last().is_none_or(|(last, _)| *last != t) {
            buckets.push((t, Charge::default()));
        }
        previous = Some((ms, row.get(1)?, row.get(2)?));
    }
    // mA·ms → Ah: / 1000 / 1000 / 3600; mV·mA·ms → Wh: / 1e9 / 3600.
    Ok(buckets
        .into_iter()
        .map(|(t, c)| {
            json!({
                "ts": bucket_ts(t),
                "ah_out": round(c.out as f64 / 3.6e9, 4),
                "ah_in": round(c.into as f64 / 3.6e9, 4),
                "wh_out": round(c.energy_out as f64 / 3.6e12, 3),
                "wh_in": round(c.energy_in as f64 / 3.6e12, 3),
            })
        })
        .collect())
}

pub type Endpoint = fn(&Connection, &Params) -> Rows;

pub fn endpoint(name: &str) -> Option<Endpoint> {
    match name {
        "probes" => Some(probes),
        "series" => Some(series),
        "energy" => Some(energy),
        _ => None,
    }
}

pub struct Api {
    db_path: String,
}

impl Api {
    pub fn query(&self, endpoint: Endpoint, params: &Params) -> Rows {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let db = Connection::open_with_flags(&self.db_path, flags)?;
        endpoint(&db, params)
    }

    fn handle(&self, mut request: Request) {
        let url = request.url().to_string();
        let (path, query) = url.split_once('?').unwrap_or((&url, ""));
        let path = path.split_once('#').map_or(path, |(p, _)| p);
        // The proxy may pass the route's trailing slash on: //probes.
        let path = format!("/{}", path.trim_matches('/'));
        let is_mcp = path == "/mcp";
        let response = match request.method() {
            Method::Get if is_mcp => not_allowed("POST"),
            Method::Get => self.get(&path, &url, query),
            Method::Post if !is_mcp => not_allowed("GET"),
            Method::Post => self.post(&mut request),
            _ => not_allowed(if is_mcp { "POST" } else { "GET" }),
        };
        let _ = request.respond(response);
    }

    fn get(&self, path: &str, url: &str, query: &str) -> Resp {
        let Some(endpoint) = endpoint(&path[1..]) else {
            return send_json(404, &json!({"error": format!("no such path {path}")}));
        };
        match self.query(endpoint, &parse_query(query)) {
            Ok(rows) => send_json(200, &json!({"rows": rows})),
            Err(ApiError::BadRequest(e)) => send_json(400, &json!({"error": e})),
            Err(ApiError::NotFound(e)) => send_json(404, &json!({"error": e})),
            Err(ApiError::Database(e)) => {
                println!("API query {url} failed: {e}");
                send_json(500, &json!({"error": "database error"}))
            }
        }
    }

    fn post(&self, request: &mut Request) -> Resp {
        let length = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("Content-Length"))
            .and_then(|h| h.value.as_str().trim().parse::<usize>().ok());
        let Some(length) = length else {
            return send_json(411, &json!({"error": "Content-Length required"}));
        };
        if length > MAX_BODY {
            return send_json(413, &json!({"error": format!("body over {MAX_BODY} bytes")}));
        }
        let mut body = Vec::with_capacity(length);
        if request.as_reader().take(length as u64).read_to_end(&mut body).is_err() {
            return send_json(400, &json!({"error": "could not read the body"}));
        }
        match mcp::handle(&body, |name, args| self.tool(name, args)) {
            (status, Some(answer)) => send_json(status, &answer),
            (status, None) => Response::from_data(Vec::new()).with_status_code(status),
        }
    }

    fn tool(&self, name: &str, args: &Params) -> Result<Vec<Value>, mcp::ToolError> {
        let endpoint = endpoint(name).expect("tool names are endpoint names");
        self.query(endpoint, args).map_err(|e| match e {
            ApiError::BadRequest(m) | ApiError::NotFound(m) => mcp::ToolError::Invalid(m),
            ApiError::Database(e) => {
                println!("MCP tool {name} {args:?} failed: {e}");
                mcp::ToolError::Internal("database error".into())
            }
        })
    }
}

type Resp = Response<std::io::Cursor<Vec<u8>>>;

fn header(field: &str, value: &str) -> Header {
    Header::from_bytes(field.as_bytes(), value.as_bytes()).expect("valid header")
}

fn send_json(status: u16, body: &Value) -> Resp {
    Response::from_data(body.to_string().into_bytes())
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json"))
}

fn not_allowed(allow: &str) -> Resp {
    Response::from_data(Vec::new()).with_status_code(405).with_header(header("Allow", allow))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = |at: usize| b.get(at).and_then(|c| (*c as char).to_digit(16));
                match (hex(i + 1), hex(i + 2)) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Query-string parameters; the last value wins and blank values count as absent.
fn parse_query(query: &str) -> Params {
    let mut params = Params::new();
    for pair in query.split(['&', ';']) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);
        if !value.is_empty() {
            params.insert(percent_decode(key), value);
        }
    }
    params
}

/// Serve the API from worker threads, which stop with the collector.
pub fn start(host: &str, port: u16, db_path: &str) -> Result<(), String> {
    let server = Arc::new(Server::http((host, port)).map_err(|e| e.to_string())?);
    let api = Arc::new(Api { db_path: db_path.to_string() });
    for _ in 0..WORKERS {
        let (server, api) = (server.clone(), api.clone());
        thread::spawn(move || {
            for request in server.incoming_requests() {
                api.handle(request);
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(readings: &[(&str, i64, i64)]) -> Connection {
        let mut store = crate::store::Store::open(":memory:").unwrap();
        for &(ts, mv, ma) in readings {
            store.add("battery".into(), ts.into(), mv, ma);
        }
        store.flush().unwrap();
        store.into_connection()
    }

    fn params(pairs: &[(&str, &str)]) -> Params {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// Readings every second from `start` for `seconds`, at `ma`.
    fn steady(start: i64, seconds: i64, ma: i64) -> Vec<(String, i64, i64)> {
        (0..seconds).map(|s| (iso(DateTime::from_timestamp(start + s, 0).unwrap()), 12000, ma)).collect()
    }

    const T0: i64 = 1_763_100_000 / 3600 * 3600;

    fn energy_rows(readings: Vec<(String, i64, i64)>, extra: &[(&str, &str)]) -> Vec<Value> {
        let readings: Vec<_> = readings.iter().map(|(t, v, i)| (t.as_str(), *v, *i)).collect();
        let db = db(&readings);
        let mut p = params(&[("probe", "battery"), ("from", &bucket_ts(T0)), ("to", &bucket_ts(T0 + 7200))]);
        p.extend(params(extra));
        energy(&db, &p).unwrap()
    }

    #[test]
    fn one_amp_for_an_hour() {
        let mut r = steady(T0, 3600, 1000);
        r.extend(steady(T0 + 3600, 1, 0));
        let rows = energy_rows(r, &[]);
        assert_eq!(rows[0]["ah_out"], json!(1.0));
        assert_eq!(rows[0]["wh_out"], json!(12.0));
        assert_eq!(rows[0]["ah_in"], json!(0.0));
    }

    #[test]
    fn half_amp_charging_for_half_an_hour() {
        let mut r = steady(T0, 1800, -500);
        r.extend(steady(T0 + 1800, 1, 0));
        let rows = energy_rows(r, &[]);
        assert_eq!(rows[0]["ah_in"], json!(0.25));
        assert_eq!(rows[0]["ah_out"], json!(0.0));
    }

    #[test]
    fn gap_counts_ten_seconds() {
        // 3.6 A, one reading, then a 5-minute gap: 3600 mA × 10 s = 0.01 Ah.
        let r = vec![(bucket_ts(T0), 12000, 3600), (bucket_ts(T0 + 300), 12000, 0)];
        let r = r.into_iter().map(|(t, v, i)| (t.replace('Z', ".000Z"), v, i)).collect();
        let rows = energy_rows(r, &[]);
        assert_eq!(rows[0]["ah_out"], json!(0.01));
    }

    #[test]
    fn reading_after_range_closes_the_last_span() {
        // The last reading inside counts up to the first one after `to`.
        let r = vec![(iso(DateTime::from_timestamp(T0 + 7195, 0).unwrap()), 12000, 3600)];
        let mut r = r;
        r.push((iso(DateTime::from_timestamp(T0 + 7200, 0).unwrap()), 12000, 0));
        let rows = energy_rows(r, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["ts"], json!(bucket_ts(T0 + 3600)));
        assert_eq!(rows[0]["ah_out"], json!(0.005));
    }

    #[test]
    fn limits_and_errors() {
        let db = db(&[]);
        let err = |p: &[(&str, &str)]| match series(&db, &params(p)) {
            Err(ApiError::BadRequest(m)) | Err(ApiError::NotFound(m)) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(err(&[]), "probe is required");
        assert_eq!(err(&[("probe", "x"), ("bucket", "1")]), "bucket must be at least 2 seconds");
        assert_eq!(err(&[("probe", "x"), ("bucket", "1.5")]), "bucket must be a whole number of seconds");
        assert_eq!(err(&[("probe", "x"), ("bucket", "2")]).split(';').next().unwrap(), "at most 5000 buckets per request");
        assert_eq!(err(&[("probe", "x"), ("from", "yesterday")]), "from must be an ISO 8601 time, like 2026-11-14T08:00:00Z");
        assert_eq!(err(&[("probe", "x"), ("from", "2026-11-14T00:00:00Z"), ("to", "2026-11-13")]), "from must be before to");
        assert_eq!(err(&[("probe", "x")]), "unknown probe 'x'");
    }

    #[test]
    fn times() {
        let t = |s| parse_iso(s).map(iso);
        assert_eq!(t("2026-11-14T08:00:00Z").unwrap(), "2026-11-14T08:00:00.000Z");
        assert_eq!(t("2026-11-14T10:00:00+02:00").unwrap(), "2026-11-14T08:00:00.000Z");
        assert_eq!(t("2026-11-14T08:00:00.5").unwrap(), "2026-11-14T08:00:00.500Z");
        assert_eq!(t("2026-11-14").unwrap(), "2026-11-14T00:00:00.000Z");
        assert_eq!(t("2026-11-14 08:00").unwrap(), "2026-11-14T08:00:00.000Z");
        assert!(t("nope").is_none());
        assert_eq!(ts_millis("2026-11-14T08:00:00.123Z"), parse_iso("2026-11-14T08:00:00.123Z").map(|d| d.timestamp_millis()));
    }

    #[test]
    fn query_strings() {
        let p = parse_query("probe=bat%20tery&from=&bucket=60&bucket=120&x=a+b");
        assert_eq!(p.get("probe").unwrap(), "bat tery");
        assert!(!p.contains_key("from"));
        assert_eq!(p.get("bucket").unwrap(), "120");
        assert_eq!(p.get("x").unwrap(), "a b");
    }

    #[test]
    fn rounding_like_python() {
        assert_eq!(round(2.675, 2), 2.67);
        assert_eq!(round(0.0005, 3), 0.001);
        assert_eq!(py_repr("it's"), "\"it's\"");
    }
}

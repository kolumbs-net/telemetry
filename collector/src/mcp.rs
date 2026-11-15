//! MCP endpoint: the API's queries as tools, over JSON-RPC 2.0.
//!
//! Implements the stateless part of MCP's Streamable HTTP transport that a
//! read-only tool server needs: every request gets one plain JSON answer, with no
//! sessions and no event stream.

use serde_json::{Map, Value, json};

use crate::api::{Params, py_repr};
use crate::store::value_text;

// Newest first; a client asking for another version gets the newest.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

pub enum ToolError {
    /// A bad argument: a tool result with isError, not a protocol error.
    Invalid(String),
    Internal(String),
}

fn rows_schema(properties: Value) -> Value {
    json!({
        "type": "object",
        "properties": {"rows": {"type": "array", "items": {"type": "object", "properties": properties}}},
        "required": ["rows"],
    })
}

fn range_input(default_bucket: u32) -> Value {
    json!({
        "type": "object",
        "properties": {
            "probe": {"type": "string", "description": "Probe name, as listed by the probes tool."},
            "from": {
                "type": "string",
                "description": "Start time, inclusive, UTC ISO 8601 (2026-11-14T08:00:00Z). Default: 24 hours before to.",
            },
            "to": {"type": "string", "description": "End time, exclusive, UTC ISO 8601. Default: now."},
            "bucket": {
                "type": "integer",
                "minimum": 2,
                "description": format!("Bucket size in seconds. Default: {default_bucket}."),
            },
        },
        "required": ["probe"],
    })
}

fn tools() -> Value {
    let number = json!({"type": "number"});
    let nullable = json!({"type": ["number", "null"]});
    json!([
        {
            "name": "probes",
            "title": "Probes",
            "description": "Every probe with its latest reading: voltage v (V), current i (A, positive = \
                discharging) and power p (W). ts is null for a probe without readings.",
            "inputSchema": {"type": "object", "properties": {}},
            "outputSchema": rows_schema(json!({
                "name": {"type": "string"},
                "ts": {"type": ["string", "null"]},
                "v": nullable, "i": nullable, "p": nullable,
            })),
        },
        {
            "name": "series",
            "title": "Voltage and current series",
            "description": "A probe's readings grouped into time buckets: per bucket the number of readings n, \
                average, minimum and maximum voltage (V) and current (A, positive = discharging), and average \
                power (W). ts is the bucket start, UTC. Buckets without readings are left out. At most 5,000 \
                buckets per call.",
            "inputSchema": range_input(60),
            "outputSchema": rows_schema(json!({
                "ts": {"type": "string"},
                "n": {"type": "integer"},
                "v_avg": number, "v_min": number, "v_max": number,
                "i_avg": number, "i_min": number, "i_max": number, "p_avg": number,
            })),
        },
        {
            "name": "energy",
            "title": "Charge and energy",
            "description": "Charge (Ah) and energy (Wh) per time bucket for a probe: ah_out/wh_out while \
                discharging, ah_in/wh_in while charging, both positive. ts is the bucket start, UTC; for local-day \
                totals sum hourly buckets. Gaps over 10 s between readings count as 10 s. At most 5,000 buckets \
                per call.",
            "inputSchema": range_input(3600),
            "outputSchema": rows_schema(json!({
                "ah_out": number, "ah_in": number, "wh_out": number, "wh_in": number,
                "ts": {"type": "string"},
            })),
        },
    ])
}

const TOOL_NAMES: [&str; 3] = ["probes", "series", "energy"];

struct RpcError(i64, String);

fn truthy(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    })
}

fn shown(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => py_repr(s),
        Some(other) => value_text(other),
    }
}

fn initialize(params: &Map<String, Value>) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested.filter(|v| PROTOCOL_VERSIONS.contains(v)).unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "telemetry", "version": "1.0.0"},
    })
}

fn call_tool<F>(params: &Map<String, Value>, call: &F) -> Result<Value, RpcError>
where
    F: Fn(&str, &Params) -> Result<Vec<Value>, ToolError>,
{
    let name = match params.get("name") {
        Some(Value::String(n)) if TOOL_NAMES.contains(&n.as_str()) => n.as_str(),
        other => return Err(RpcError(INVALID_PARAMS, format!("unknown tool {}", shown(other)))),
    };
    let empty = Map::new();
    let arguments = match truthy(params.get("arguments")) {
        None => &empty,
        Some(Value::Object(a)) => a,
        Some(_) => return Err(RpcError(INVALID_PARAMS, "arguments must be an object".into())),
    };
    // The REST API's checks expect query-string values.
    let args: Params = arguments
        .iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| (k.clone(), value_text(v)))
        .collect();
    match call(name, &args) {
        Ok(rows) => {
            let result = json!({"rows": rows});
            Ok(json!({"content": [{"type": "text", "text": result.to_string()}], "structuredContent": result}))
        }
        Err(ToolError::Invalid(message)) => Ok(json!({"content": [{"type": "text", "text": message}], "isError": true})),
        Err(ToolError::Internal(message)) => Err(RpcError(INTERNAL_ERROR, message)),
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Answer one POSTed JSON-RPC message: (HTTP status, JSON body or None).
///
/// call(tool, args) runs an API query and returns its rows.
pub fn handle<F>(body: &[u8], call: F) -> (u16, Option<Value>)
where
    F: Fn(&str, &Params) -> Result<Vec<Value>, ToolError>,
{
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return (400, Some(error(Value::Null, PARSE_ERROR, "body is not JSON")));
    };
    let Some(message) = message.as_object().filter(|m| m.get("jsonrpc") == Some(&json!("2.0"))) else {
        return (400, Some(error(Value::Null, INVALID_REQUEST, "expected one JSON-RPC 2.0 message")));
    };
    let (Some(id), Some(method)) = (message.get("id"), message.get("method")) else {
        // Notifications, and responses to requests this server never sends.
        return (202, None);
    };
    let Some(method) = method.as_str() else {
        return (400, Some(error(Value::Null, INVALID_REQUEST, "method must be a string")));
    };
    let id = id.clone();
    let empty = Map::new();
    let params = match truthy(message.get("params")) {
        None => &empty,
        Some(Value::Object(p)) => p,
        Some(_) => return (200, Some(error(id, INVALID_PARAMS, "params must be an object"))),
    };
    let result = match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => call_tool(params, &call),
        _ => {
            let message = format!("method {} not supported", py_repr(method));
            return (200, Some(error(id, METHOD_NOT_FOUND, &message)));
        }
    };
    match result {
        Ok(result) => (200, Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))),
        Err(RpcError(code, message)) => (200, Some(error(id, code, &message))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(body: &str) -> (u16, Option<Value>) {
        handle(body.as_bytes(), |name, args| match args.get("bucket").map(String::as_str) {
            Some("1") => Err(ToolError::Invalid("bucket must be at least 2 seconds".into())),
            _ => Ok(vec![json!({"tool": name})]),
        })
    }

    #[test]
    fn initialize_picks_a_version() {
        let (_, body) = rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#);
        assert_eq!(body.unwrap()["result"]["protocolVersion"], "2025-06-18");
        let (_, body) = rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999"}}"#);
        assert_eq!(body.unwrap()["result"]["protocolVersion"], "2025-11-25");
    }

    #[test]
    fn lists_three_tools() {
        let (status, body) = rpc(r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#);
        assert_eq!(status, 200);
        assert_eq!(body.unwrap()["result"]["tools"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn calls_and_errors() {
        let (_, body) = rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"energy","arguments":{"probe":"b","bucket":60}}}"#);
        let result = &body.unwrap()["result"];
        assert_eq!(result["structuredContent"]["rows"][0]["tool"], "energy");
        let (_, body) = rpc(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"energy","arguments":{"bucket":1}}}"#);
        assert_eq!(body.unwrap()["result"]["isError"], true);
        let (_, body) = rpc(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}}"#);
        assert_eq!(body.unwrap()["error"]["message"], "unknown tool 'nope'");
        let (_, body) = rpc(r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#);
        assert_eq!(body.unwrap()["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn transport_cases() {
        assert_eq!(rpc(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), (202, None));
        assert_eq!(rpc("nope").0, 400);
        assert_eq!(rpc(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#).0, 400);
        assert_eq!(rpc(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).1.unwrap()["result"], json!({}));
    }
}

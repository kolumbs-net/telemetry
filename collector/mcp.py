"""MCP endpoint: the API's queries as tools, over JSON-RPC 2.0.

Implements the stateless part of MCP's Streamable HTTP transport that a
read-only tool server needs: every request gets one plain JSON answer, with no
sessions and no event stream.
"""

import json

# Newest first; a client asking for another version gets the newest.
PROTOCOL_VERSIONS = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"]
SERVER_INFO = {"name": "telemetry", "version": "1.0.0"}

RANGE_PROPERTIES = {
    "probe": {"type": "string", "description": "Probe name, as listed by the probes tool."},
    "from": {
        "type": "string",
        "description": "Start time, inclusive, UTC ISO 8601 (2026-11-14T08:00:00Z). Default: 24 hours before to.",
    },
    "to": {"type": "string", "description": "End time, exclusive, UTC ISO 8601. Default: now."},
}


def rows_schema(properties):
    return {
        "type": "object",
        "properties": {"rows": {"type": "array", "items": {"type": "object", "properties": properties}}},
        "required": ["rows"],
    }


NUMBER = {"type": "number"}
NULLABLE_NUMBER = {"type": ["number", "null"]}

TOOLS = [
    {
        "name": "probes",
        "title": "Probes",
        "description": "Every probe with its latest reading: voltage v (V), current i (A, positive = "
        "discharging) and power p (W). ts is null for a probe without readings.",
        "inputSchema": {"type": "object", "properties": {}},
        "outputSchema": rows_schema({
            "name": {"type": "string"},
            "ts": {"type": ["string", "null"]},
            "v": NULLABLE_NUMBER,
            "i": NULLABLE_NUMBER,
            "p": NULLABLE_NUMBER,
        }),
    },
    {
        "name": "series",
        "title": "Voltage and current series",
        "description": "A probe's readings grouped into time buckets: per bucket the number of readings n, "
        "average, minimum and maximum voltage (V) and current (A, positive = discharging), and average "
        "power (W). ts is the bucket start, UTC. Buckets without readings are left out. At most 5,000 "
        "buckets per call.",
        "inputSchema": {
            "type": "object",
            "properties": {
                **RANGE_PROPERTIES,
                "bucket": {"type": "integer", "minimum": 2, "description": "Bucket size in seconds. Default: 60."},
            },
            "required": ["probe"],
        },
        "outputSchema": rows_schema({
            "ts": {"type": "string"},
            "n": {"type": "integer"},
            **{k: NUMBER for k in ("v_avg", "v_min", "v_max", "i_avg", "i_min", "i_max", "p_avg")},
        }),
    },
    {
        "name": "energy",
        "title": "Charge and energy",
        "description": "Charge (Ah) and energy (Wh) per time bucket for a probe: ah_out/wh_out while "
        "discharging, ah_in/wh_in while charging, both positive. ts is the bucket start, UTC; for local-day "
        "totals sum hourly buckets. Gaps over 10 s between readings count as 10 s. At most 5,000 buckets "
        "per call.",
        "inputSchema": {
            "type": "object",
            "properties": {
                **RANGE_PROPERTIES,
                "bucket": {"type": "integer", "minimum": 2, "description": "Bucket size in seconds. Default: 3600."},
            },
            "required": ["probe"],
        },
        "outputSchema": rows_schema({k: NUMBER for k in ("ah_out", "ah_in", "wh_out", "wh_in")} | {"ts": {"type": "string"}}),
    },
]
TOOL_NAMES = {tool["name"] for tool in TOOLS}

PARSE_ERROR = -32700
INVALID_REQUEST = -32600
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603


class RpcError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def initialize(params, call):
    requested = params.get("protocolVersion")
    version = requested if requested in PROTOCOL_VERSIONS else PROTOCOL_VERSIONS[0]
    return {"protocolVersion": version, "capabilities": {"tools": {}}, "serverInfo": SERVER_INFO}


def list_tools(params, call):
    return {"tools": TOOLS}


def call_tool(params, call):
    name = params.get("name")
    if name not in TOOL_NAMES:
        raise RpcError(INVALID_PARAMS, f"unknown tool {name!r}")
    arguments = params.get("arguments") or {}
    if not isinstance(arguments, dict):
        raise RpcError(INVALID_PARAMS, "arguments must be an object")
    # The REST API's checks expect query-string values.
    args = {k: str(v) for k, v in arguments.items() if v is not None}
    try:
        rows = call(name, args)
    except (ValueError, LookupError) as e:
        return {"content": [{"type": "text", "text": str(e)}], "isError": True}
    except RuntimeError as e:
        raise RpcError(INTERNAL_ERROR, str(e)) from e
    result = {"rows": rows}
    return {"content": [{"type": "text", "text": json.dumps(result)}], "structuredContent": result}


METHODS = {
    "initialize": initialize,
    "ping": lambda params, call: {},
    "tools/list": list_tools,
    "tools/call": call_tool,
}


def error(id_, code, message):
    return {"jsonrpc": "2.0", "id": id_, "error": {"code": code, "message": message}}


def handle(body, call):
    """Answer one POSTed JSON-RPC message: (HTTP status, JSON body or None).

    call(tool, args) runs an API query and returns its rows.
    """
    try:
        message = json.loads(body)
    except ValueError:
        return 400, error(None, PARSE_ERROR, "body is not JSON")
    if not isinstance(message, dict) or message.get("jsonrpc") != "2.0":
        return 400, error(None, INVALID_REQUEST, "expected one JSON-RPC 2.0 message")
    if "id" not in message or "method" not in message:
        # Notifications, and responses to requests this server never sends.
        return 202, None
    if not isinstance(message["method"], str):
        return 400, error(None, INVALID_REQUEST, "method must be a string")
    id_ = message["id"]
    method = METHODS.get(message["method"])
    if method is None:
        return 200, error(id_, METHOD_NOT_FOUND, f"method {message['method']!r} not supported")
    params = message.get("params") or {}
    if not isinstance(params, dict):
        return 200, error(id_, INVALID_PARAMS, "params must be an object")
    try:
        return 200, {"jsonrpc": "2.0", "id": id_, "result": method(params, call)}
    except RpcError as e:
        return 200, error(id_, e.code, str(e))

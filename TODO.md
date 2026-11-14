# TODO

## Public read-only REST API

**Today:** readings are stored in SQLite on the server (`probes`, `readings`;
see the README's Storage section), but they can only be read with `sqlite3` on
the server itself. Dashboards with graphs need a data source they can reach
from anywhere.

**Decided:**

- REST with JSON. Not GraphQL: the data is one table, and every view a
  dashboard needs (latest reading, chart series, charge totals) is a
  calculation on the server either way. Not Grafana: the dashboard is the
  claude.ai one, which reads the data through the MCP connector below.
- Public and read-only, served through an Apache route on the server's TLS
  vhost (`<route>`). The API's port stays local.
- Standard library only (`http.server`, `sqlite3`), so the release stays a bare
  venv on the 32-bit Pi.
- No CORS: the dashboard reaches the data through the MCP connector, whose
  calls come from claude.ai's servers, not from a browser.

**Design:**

- Runs **inside the collector process**, in a background thread
  (`ThreadingHTTPServer`), on **its own TCP port bound to localhost**, separate
  from the collector's UDP port. One process because the database belongs to
  the `telemetry` account: a second service in its own slot couldn't read it
  under the slot's `ProtectSystem=strict`.
- Opens SQLite **read-only** per request (`file:…?mode=ro`); WAL lets it read
  while the collector writes.
- New required `TELEMETRY_API_PORT`, next to `TELEMETRY_UDP_PORT` and
  `TELEMETRY_DB`. On SIGTERM the collector flushes as now; the API thread is a
  daemon and stops with it.

**Endpoints** (all `GET`, JSON, times as UTC ISO `…Z`):

| Endpoint | Returns |
|---|---|
| `/probes` | every probe with its latest reading: `name`, `ts`, `v`, `i`, `p` |
| `/series?probe=&from=&to=&bucket=` | readings grouped into buckets of `bucket` seconds (default 60): `ts` (bucket start), `n`, `v_avg/v_min/v_max`, `i_avg/i_min/i_max`, `p_avg` |
| `/energy?probe=&from=&to=&bucket=` | per bucket (default 3600 s): `ah_out`, `ah_in`, `wh_out`, `wh_in`; out = discharging (positive current), in = charging |

- `from`/`to` default to the last 24 hours. `probe` is required for `/series`
  and `/energy`.
- Units: V, A, W, Ah, Wh. `p_avg` is the average of `mv × ma` per reading.
- Buckets in SQL: `CAST(strftime('%s', ts) AS INTEGER) / bucket * bucket`, with
  the time range filtered on the `(probe_id, ts)` key, so a query reads only its
  range. (The server's SQLite is too old for `unixepoch(…, 'subsec')`; use
  `julianday`.)
- Charge is integrated in SQL: each reading's current × time to the next
  reading (`LEAD`). Gaps over 10 s count as 10 s, so an outage isn't counted as
  current flowing all along.
- Buckets are aligned to UTC. For local-day totals, a client sums hourly
  buckets per local day; time zone support can come later.
- Limits that protect the Pi: at most 5,000 buckets per response, bucket at
  least 2 s; otherwise `400`.
- Errors: `400 {"error": …}` for bad parameters, `404` for an unknown probe or
  path, `405` for anything but GET.
- No access log: Apache logs requests; the journal keeps startup and errors.

**Depends on** infra-orchestrator's TODO "Standard ports for service slots":
the slot then gives this service `COLLECTOR_PORT` (udp, internal) and
`API_PORT` (tcp, external, with an `http` route), and `PORT` goes away.

**How:**

1. `collector/api.py`: handler, endpoints, parameter checks, SQL.
2. `collector/server.py`: start the API thread; one more startup log line.
3. `deploy/start`: `TELEMETRY_UDP_PORT="$COLLECTOR_PORT"` (instead of `$PORT`)
   and `TELEMETRY_API_PORT="$API_PORT"`.
4. README: an "API" section with endpoints, parameters, units, limits and
   example `curl` calls, using `<route>` for the public path.
5. Merge right after infra's change is deployed. Until then the collector
   stops, since it still expects `PORT`; a short gap in readings is acceptable.
6. Check by hand: a temporary database with known readings (1 A for an hour →
   `ah_out` 1.0; −0.5 A for 30 minutes → `ah_in` 0.25; a 5-minute gap counts
   10 s). `curl` every endpoint, including the limits and error codes, while UDP
   readings keep being stored. On the server, check response times for 24 hours
   and 7 days (`curl -w '%{time_total}'`); aim for under about 2 s.

## MCP connector for the dashboard

**Today:** the claude.ai dashboard ("Battery Telemetry") reads a CSV that is
exported from a copy of `telemetry.db` and uploaded by hand. Every refresh
takes a backup, a copy and an upload, and a copy of the main file alone misses
what is still in the WAL.

**Decided:**

- The dashboard reads live data through an MCP connector that the collector
  serves. A claude.ai dashboard can't call a URL itself; it reaches live data
  only through a connector.
- The same data and SQL as the REST API: the MCP tools are a second way into
  `collector/api.py`'s queries, not new calculations.
- Public, read-only and without login, like the REST API; added in claude.ai
  as a custom connector without authentication.
- Standard library only. MCP's Streamable HTTP transport is JSON-RPC 2.0 over
  HTTP POST, which `http.server` handles; answers are plain JSON, no event
  stream.

**Design:**

- `POST /mcp` on the API's port, behind the same Apache route
  (`<route>/mcp`). It answers `initialize`, `tools/list`, `tools/call` and
  `ping`; `notifications/initialized` gets `202` with no body; any other method
  gets JSON-RPC error `-32601`. `GET /mcp` gets `405` (no server-sent events).
- `initialize` answers with the client's `protocolVersion` if the server
  supports it, else the newest one it knows.
- Tools mirror the endpoints: `probes`, `series` (`probe`, `from`, `to`,
  `bucket`) and `energy` (the same). Each returns `{"rows": [...]}`, as text
  content and as `structuredContent`, so the dashboard reads it as rows without
  a loader.
- The same parameter checks and limits as REST (at most 5,000 buckets, bucket
  at least 2 s). A bad parameter is a tool result with `isError: true` and the
  message the REST `400` gives.

**Dashboard:** its data becomes live queries on the connector: `series` with
`bucket` 60 for voltage and current, `energy` with `bucket` 60 for charge.
A date range filter defaults to the last 3 days, which stays under 5,000
one-minute buckets; a longer range uses 10-minute or hourly buckets. Each open
or refresh reads the latest data, and the CSV and its hand refresh go away.

**Depends on** the REST API above: its queries, its port and its route.

**How:**

1. `collector/mcp.py`: JSON-RPC handling, the tool list with input schemas,
   calls into `api.py`.
2. `collector/api.py`: send `POST /mcp` to it.
3. README: an "MCP connector" section with the URL (`<route>/mcp`), the tools,
   and how to add it in claude.ai.
4. Check by hand: `curl` `initialize`, `tools/list` and each tool, including a
   bad parameter. Then add the connector in claude.ai and call each tool once
   from a chat.
5. Switch the dashboard's data to the connector and remove its CSV file.

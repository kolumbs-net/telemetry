# telemetry

Current and voltage metering for the off-grid solar system.

**Probes** are small Arduino devices with an INA219 sensor, clipped onto a wire
(battery, consumers, ...). They send readings over UDP to the **collector**,
which stores them and serves them over a read-only JSON API.

```
probe (Arduino Nano + INA219 + ENC28J60)  --UDP-->  collector (SQLite)  --HTTP-->  API
```

## Layout

```
telemetry/
├── firmware/probe/   # Arduino sketch, one firmware for all probes
└── collector/        # UDP listener, storage and API
```

## Configuration

No network details are committed to this repository.

- **Probe:** copy `firmware/probe/config.example.h` to `firmware/probe/config.h`
  and fill in the probe's name, static IP, MAC, gateway, DNS and the collector's
  address and port. `config.h` is gitignored.
- **Collector:** set `TELEMETRY_UDP_PORT` to the UDP port to listen on,
  `TELEMETRY_API_PORT` to the TCP port the API listens on (localhost only), and
  `TELEMETRY_DB` to the SQLite file to store readings in.

## Payload

```json
{"probe": "battery", "v": 12.85, "i": 1.234}
```

`probe` the probe's name (`PROBE_NAME` in `config.h`), `v` bus voltage (V),
`i` current (A). Positive current means discharging, negative means charging.
Power is not sent; it is `v × i`.

Packets without `probe` are stored under their sender address.

## Firmware

Build:

```bash
arduino-cli compile --fqbn arduino:avr:nano:cpu=atmega328 firmware/probe
```

Deploy:

```bash
arduino-cli upload -p /dev/ttyUSB0 --fqbn arduino:avr:nano:cpu=atmega328 --upload-property upload.speed=115200 firmware/probe
```

Serial inspect:

```bash
picocom -b 9600 /dev/ttyUSB0
```

## Collector

Run by hand, for testing:

```bash
TELEMETRY_UDP_PORT=<port> TELEMETRY_API_PORT=<port> TELEMETRY_DB=telemetry.db \
  python3 collector/server.py
```

It logs only its startup and errors (such as malformed packets).

## API

The collector serves its readings as JSON, read-only, from a thread of its own
process. It listens on localhost only; on the server, Apache publishes it on
the TLS vhost at `<route>`. Readings reach the API when they are stored, so
the newest may be up to a minute old (see Storage).

All endpoints are `GET`. Times are UTC ISO 8601 (`2026-11-14T08:00:00Z`; a
time without an offset is taken as UTC). Every answer is `{"rows": [...]}`.

| Endpoint | Rows |
|---|---|
| `/probes` | every probe with its latest reading: `name`, `ts`, `v`, `i`, `p` |
| `/series?probe=&from=&to=&bucket=` | readings per bucket of `bucket` seconds (default 60): `ts` (bucket start), `n`, `v_avg`, `v_min`, `v_max`, `i_avg`, `i_min`, `i_max`, `p_avg` |
| `/energy?probe=&from=&to=&bucket=` | charge and energy per bucket (default 3600 s): `ah_out`, `ah_in`, `wh_out`, `wh_in` |

- `probe` (the probe's name) is required for `/series` and `/energy`.
  `from`/`to` default to the last 24 hours; `from` is inclusive, `to` exclusive.
- Units: V, A, W, Ah, Wh. `i` is positive when discharging. `p_avg` is the
  average of each reading's `v × i`.
- Buckets are aligned to UTC. For local-day totals, sum hourly buckets per
  local day. Buckets without readings are left out.
- Charge counts each reading's current until the next reading. A gap of more
  than 10 seconds counts as 10 seconds, so an outage isn't counted as current
  flowing all along. Out = discharging, in = charging; both are positive.
- Limits: at most 5,000 buckets per request, and a bucket of at least 2
  seconds.
- Errors are `{"error": "..."}`: `400` for a bad parameter, `404` for an
  unknown probe or path, `405` for any method but `GET`.

```bash
curl https://<host><route>/probes
curl "https://<host><route>/series?probe=battery&from=2026-11-14T00:00:00Z&bucket=300"
curl "https://<host><route>/energy?probe=battery&from=2026-11-07T00:00:00Z"
```

## MCP connector

The same queries are tools on an MCP server at `<route>/mcp`, so an MCP client
such as claude.ai can read live data. It speaks MCP's Streamable HTTP
transport in its simplest form: each `POST` carries one JSON-RPC 2.0 message
and gets one JSON answer. It needs no login and keeps no sessions. A `GET`
gets `405`, because the server sends no events.

| Tool | Arguments | Result |
|---|---|---|
| `probes` | none | the rows of `/probes` |
| `series` | `probe`, `from`, `to`, `bucket` (default 60) | the rows of `/series` |
| `energy` | `probe`, `from`, `to`, `bucket` (default 3600) | the rows of `/energy` |

Each tool returns `{"rows": [...]}` as structured content and as JSON text.
The tools use the same checks and limits as the API, and a bad argument gives a
tool result with `isError` and the API's error message.

To add it in claude.ai: Settings → Connectors → Add custom connector, with the
URL `https://<host><route>/mcp` and no authentication.

```bash
curl https://<host><route>/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"energy","arguments":{"probe":"battery","bucket":86400}}}'
```

## Storage

Readings are kept forever in SQLite (`TELEMETRY_DB`; on the server
`/var/lib/telemetry/telemetry.db`). They are written in batches every 60
seconds and when the collector stops, to spare the SD card: a power loss loses
at most the last minute.

```sql
probes   (id, name)                -- one row per probe
readings (probe_id, ts, mv, ma)    -- one row per reading
```

| Column | Meaning |
|---|---|
| `ts` | receive time, UTC ISO 8601 with milliseconds, e.g. `2026-10-18T07:17:21.123Z` |
| `mv` | bus voltage in millivolts |
| `ma` | current in milliamps: positive = discharging, negative = charging |

Power (`mv * ma / 1e6` W) and amp-hours are computed from the readings, not
stored. `ts` has a fixed format, so comparing it as text compares times.

On the server, query as the `telemetry` account, so SQLite's side files keep
their owner:

```bash
sudo -u telemetry sqlite3 /var/lib/telemetry/telemetry.db
```

```sql
-- readings per probe, and the latest one
SELECT p.name, count(*), max(r.ts)
FROM readings r JOIN probes p ON p.id = r.probe_id
GROUP BY p.name;

-- the last hour, in volts, amps and watts
SELECT p.name, r.ts, r.mv / 1000.0 AS v, r.ma / 1000.0 AS a,
       round(r.mv * r.ma / 1e6, 3) AS w
FROM readings r JOIN probes p ON p.id = r.probe_id
WHERE r.ts >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-1 hour')
ORDER BY r.ts;
```

## Deployment

The collector runs on the main server as the `telemetry` systemd service, in a
service slot provided by
[infra-orchestrator](https://github.com/kolumbs-net/infra-orchestrator), which
also allocates its ports and the API's Apache route.

Every push to `main` that changes `collector/`, `deploy/` or the workflow runs
`.github/workflows/deploy.yml` on the server's self-hosted runner:

1. `deploy/build` builds a release in `/opt/telemetry/releases/`
2. `/opt/telemetry/current` is switched to it and the service restarts
3. the workflow checks the service stays up, and keeps the last three releases

`deploy/start` is what systemd runs: it passes the slot's `PORT` to the
collector as `TELEMETRY_UDP_PORT`; the slot sets `TELEMETRY_API_PORT` itself.

Firmware-only changes do not deploy. A deploy can also be started by hand from
the Actions tab (`workflow_dispatch`).

On the server:

```bash
systemctl status telemetry
journalctl -u telemetry -f
```

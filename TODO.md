# TODO

## Public read-only REST API

**Today:** readings are stored in SQLite on the server (`probes`, `readings`;
see the README's Storage section), but they can only be read with `sqlite3` on
the server itself. Dashboards with graphs need a data source they can reach
from anywhere.

**Decided:**

- REST with JSON. Not GraphQL: the data is one table, and every view a
  dashboard needs (latest reading, chart series, charge totals) is a
  calculation on the server either way. Not Grafana: the dashboard will be our
  own page.
- Public and read-only, served through an Apache route on the server's TLS
  vhost (`<route>`). The API's port stays local.
- Standard library only (`http.server`, `sqlite3`), so the release stays a bare
  venv on the 32-bit Pi.
- No CORS until it's known where the dashboard lives.

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

## Probe names in the firmware

**Today:** the probe sends `{"v":…,"i":…,"p":…}` with no name, so the collector
stores its readings under the sender's address, which a public API would then
show. `p` is sent but never stored, since power is `v × i`.

**Plan:**

- `config.example.h` and the local `config.h`: `#define PROBE_NAME "battery"`.
- `probe.ino`: send `{"probe":"<PROBE_NAME>","v":…,"i":…}`; drop `p` and the
  `power_W` calculation. The serial output shows the same payload.
- README: the Payload section shows the new format.
- No collector change: it already uses `"probe"` when present.
- Build check: `arduino-cli compile` with the real `config.h` and with
  `config.example.h` copied as `config.h`.

**Switching over on the server** (losing a minute or two of readings is fine),
as the `telemetry` account (`sudo -u telemetry sqlite3 …`):

1. Right before flashing, give the stored readings the new name:
   ```sql
   UPDATE probes SET name = 'battery' WHERE name = '<probe IP>';
   ```
2. Flash the probe (README's upload command). Readings now arrive as `battery`
   and land on the same probe.
3. A minute or more later, drop whatever the old firmware sent in between:
   ```sql
   DELETE FROM readings WHERE probe_id = (SELECT id FROM probes WHERE name = '<probe IP>');
   DELETE FROM probes WHERE name = '<probe IP>';
   ```
   If SQLite says "database is locked", run it again.

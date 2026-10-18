# telemetry

Current and voltage metering for the off-grid solar system.

**Probes** are small Arduino devices with an INA219 sensor, clipped onto a wire
(battery, consumers, ...). They send readings over UDP to the **collector**,
which will store them and expose them over an API.

```
probe (Arduino Nano + INA219 + ENC28J60)  --UDP-->  collector
```

## Layout

```
telemetry/
├── firmware/probe/   # Arduino sketch, one firmware for all probes
└── collector/        # UDP listener
```

## Configuration

No network details are committed to this repository.

- **Probe:** copy `firmware/probe/config.example.h` to `firmware/probe/config.h`
  and fill in the probe's name, static IP, MAC, gateway, DNS and the collector's
  address and port. `config.h` is gitignored.
- **Collector:** set `TELEMETRY_UDP_PORT` to the UDP port to listen on, and
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
TELEMETRY_UDP_PORT=<port> TELEMETRY_DB=telemetry.db python3 collector/server.py
```

It logs only its startup and errors (such as malformed packets).

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
also allocates its port.

Every push to `main` that changes `collector/`, `deploy/` or the workflow runs
`.github/workflows/deploy.yml` on the server's self-hosted runner:

1. `deploy/build` builds a release in `/opt/telemetry/releases/`
2. `/opt/telemetry/current` is switched to it and the service restarts
3. the workflow checks the service stays up, and keeps the last three releases

`deploy/start` is what systemd runs: it passes the slot's `PORT` to the
collector as `TELEMETRY_UDP_PORT`.

Firmware-only changes do not deploy. A deploy can also be started by hand from
the Actions tab (`workflow_dispatch`).

On the server:

```bash
systemctl status telemetry
journalctl -u telemetry -f
```

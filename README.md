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
  and fill in the probe's static IP, MAC, gateway, DNS and the collector's
  address and port. `config.h` is gitignored.
- **Collector:** set `TELEMETRY_UDP_PORT` to the UDP port to listen on.

## Payload

```json
{"v": 12.85, "i": 1.234, "p": 15.857}
```

`v` bus voltage (V), `i` current (A), `p` power (W).

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
TELEMETRY_UDP_PORT=<port> python3 collector/server.py
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

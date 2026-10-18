"""SQLite storage for probe readings."""

import json
import sqlite3
from datetime import datetime, timezone

SCHEMA = """
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
"""


def utc_now():
    """Current time as fixed-format UTC ISO 8601 with milliseconds."""
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def parse_reading(data, sender):
    """Turn a probe packet into (probe, mv, ma).

    Packets are JSON like {"v": 12.03, "i": -1.25, "p": -15.04}; power is not
    stored, since it is v * i. Probes that send no "probe" name are named by
    their sender address.
    """
    try:
        reading = json.loads(data)
        mv = round(float(reading["v"]) * 1000)
        ma = round(float(reading["i"]) * 1000)
    except (ValueError, KeyError, TypeError, OverflowError) as e:
        raise ValueError(f"malformed reading {data[:80]!r}") from e
    probe = reading.get("probe") or sender
    return str(probe), mv, ma


class Store:
    """Buffers readings in memory and writes them to SQLite in batches."""

    def __init__(self, path):
        self.db = sqlite3.connect(path)
        # After a power loss the database stays consistent; at worst the last
        # batch is missing.
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.execute("PRAGMA synchronous=NORMAL")
        if self.db.execute("PRAGMA user_version").fetchone()[0] == 0:
            self.db.executescript(SCHEMA)
        self.pending = []

    def add(self, probe, ts, mv, ma):
        self.pending.append((probe, ts, mv, ma))

    def flush(self):
        """Write buffered readings in one transaction.

        On failure they stay buffered, and the next flush tries again.
        """
        if not self.pending:
            return
        with self.db:
            ids = {}
            for name in {probe for probe, _, _, _ in self.pending}:
                self.db.execute("INSERT OR IGNORE INTO probes (name) VALUES (?)", (name,))
                ids[name] = self.db.execute(
                    "SELECT id FROM probes WHERE name = ?", (name,)
                ).fetchone()[0]
            self.db.executemany(
                "INSERT OR IGNORE INTO readings (probe_id, ts, mv, ma) VALUES (?, ?, ?, ?)",
                [(ids[probe], ts, mv, ma) for probe, ts, mv, ma in self.pending],
            )
        self.pending.clear()

    def close(self):
        self.db.close()

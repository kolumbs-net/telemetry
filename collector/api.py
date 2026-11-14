"""Read-only JSON API over the stored readings.

Runs in a background thread of the collector and opens the database read-only
for each request, so it never blocks or alters what the collector writes.
"""

import json
import sqlite3
import threading
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

MAX_BUCKETS = 5000
MIN_BUCKET = 2
DEFAULT_RANGE = timedelta(hours=24)
# Longer gaps between readings count as this long, so an outage isn't counted
# as current flowing all along.
MAX_GAP_SECONDS = 10


class BadRequest(ValueError):
    pass


class NotFound(LookupError):
    pass


def iso(dt):
    return dt.astimezone(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def bucket_ts(seconds):
    return datetime.fromtimestamp(seconds, timezone.utc).isoformat().replace("+00:00", "Z")


def parse_time(name, value):
    try:
        dt = datetime.fromisoformat(value)
    except ValueError:
        raise BadRequest(f"{name} must be an ISO 8601 time, like 2026-11-14T08:00:00Z") from None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def time_range(params):
    to = parse_time("to", params["to"]) if "to" in params else datetime.now(timezone.utc)
    start = parse_time("from", params["from"]) if "from" in params else to - DEFAULT_RANGE
    if start >= to:
        raise BadRequest("from must be before to")
    return start, to


def bucket_size(params, default):
    if "bucket" not in params:
        return default
    try:
        bucket = int(params["bucket"])
    except ValueError:
        raise BadRequest("bucket must be a whole number of seconds") from None
    if bucket < MIN_BUCKET:
        raise BadRequest(f"bucket must be at least {MIN_BUCKET} seconds")
    return bucket


def range_query(db, params, default_bucket):
    """Common parameters of /series and /energy: (probe_id, from, to, bucket)."""
    if not params.get("probe"):
        raise BadRequest("probe is required")
    start, to = time_range(params)
    bucket = bucket_size(params, default_bucket)
    first = int(start.timestamp()) // bucket
    last = -(-int(to.timestamp()) // bucket)
    if last - first > MAX_BUCKETS:
        raise BadRequest(f"at most {MAX_BUCKETS} buckets per request; use a larger bucket or a shorter range")
    row = db.execute("SELECT id FROM probes WHERE name = ?", (params["probe"],)).fetchone()
    if row is None:
        raise NotFound(f"unknown probe {params['probe']!r}")
    return row[0], iso(start), iso(to), bucket


def probes(db, params):
    rows = db.execute(
        """
        SELECT p.name, r.ts, r.mv, r.ma
        FROM probes p
        LEFT JOIN readings r ON r.probe_id = p.id
            AND r.ts = (SELECT max(ts) FROM readings WHERE probe_id = p.id)
        ORDER BY p.name
        """
    )
    return [
        {
            "name": name,
            "ts": ts,
            "v": None if mv is None else mv / 1000,
            "i": None if ma is None else ma / 1000,
            "p": None if mv is None else round(mv * ma / 1e6, 3),
        }
        for name, ts, mv, ma in rows
    ]


def series(db, params):
    probe_id, start, to, bucket = range_query(db, params, 60)
    rows = db.execute(
        """
        SELECT CAST(strftime('%s', ts) AS INTEGER) / :bucket * :bucket AS t,
               count(*), avg(mv), min(mv), max(mv), avg(ma), min(ma), max(ma), avg(mv * ma)
        FROM readings
        WHERE probe_id = :probe AND ts >= :from AND ts < :to
        GROUP BY t
        ORDER BY t
        """,
        {"probe": probe_id, "from": start, "to": to, "bucket": bucket},
    )
    return [
        {
            "ts": bucket_ts(t),
            "n": n,
            "v_avg": round(v_avg / 1000, 3),
            "v_min": v_min / 1000,
            "v_max": v_max / 1000,
            "i_avg": round(i_avg / 1000, 3),
            "i_min": i_min / 1000,
            "i_max": i_max / 1000,
            "p_avg": round(p_avg / 1e6, 3),
        }
        for t, n, v_avg, v_min, v_max, i_avg, i_min, i_max, p_avg in rows
    ]


def energy(db, params):
    probe_id, start, to, bucket = range_query(db, params, 3600)
    # Each reading counts until the next one, so the window also takes the
    # first reading after the range. The latest reading counts once the next
    # one has arrived.
    rows = db.execute(
        """
        WITH spans AS (
            SELECT ts, mv, ma,
                   min((julianday(LEAD(ts) OVER (ORDER BY ts)) - julianday(ts)) * 86400.0,
                       :max_gap) AS dt
            FROM readings
            WHERE probe_id = :probe AND ts >= :from
              AND ts <= coalesce(
                  (SELECT min(ts) FROM readings WHERE probe_id = :probe AND ts >= :to), :to)
        )
        SELECT CAST(strftime('%s', ts) AS INTEGER) / :bucket * :bucket AS t,
               coalesce(sum(CASE WHEN ma > 0 THEN ma * dt END), 0),
               coalesce(sum(CASE WHEN ma < 0 THEN -ma * dt END), 0),
               coalesce(sum(CASE WHEN ma > 0 THEN mv * ma * dt END), 0),
               coalesce(sum(CASE WHEN ma < 0 THEN -mv * ma * dt END), 0)
        FROM spans
        WHERE ts < :to
        GROUP BY t
        ORDER BY t
        """,
        {"probe": probe_id, "from": start, "to": to, "bucket": bucket, "max_gap": MAX_GAP_SECONDS},
    )
    # mA·s → Ah: / 1000 / 3600; mV·mA·s → Wh: / 1e6 / 3600.
    return [
        {
            "ts": bucket_ts(t),
            "ah_out": round(ah_out / 3.6e6, 4),
            "ah_in": round(ah_in / 3.6e6, 4),
            "wh_out": round(wh_out / 3.6e9, 3),
            "wh_in": round(wh_in / 3.6e9, 3),
        }
        for t, ah_out, ah_in, wh_out, wh_in in rows
    ]


ENDPOINTS = {"/probes": probes, "/series": series, "/energy": energy}


def make_handler(db_path):
    db_uri = Path(db_path).resolve().as_uri() + "?mode=ro"

    class Handler(BaseHTTPRequestHandler):
        server_version = "telemetry"

        def send_json(self, status, body):
            data = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            url = urlsplit(self.path)
            # The proxy may pass the route's trailing slash on: //probes.
            path = "/" + url.path.strip("/")
            endpoint = ENDPOINTS.get(path)
            if endpoint is None:
                self.send_json(404, {"error": f"no such path {path}"})
                return
            params = {k: v[-1] for k, v in parse_qs(url.query).items()}
            try:
                db = sqlite3.connect(db_uri, uri=True)
                try:
                    rows = endpoint(db, params)
                finally:
                    db.close()
            except BadRequest as e:
                self.send_json(400, {"error": str(e)})
            except NotFound as e:
                self.send_json(404, {"error": str(e)})
            except sqlite3.Error as e:
                print(f"API query {self.path} failed: {e}")
                self.send_json(500, {"error": "database error"})
            else:
                self.send_json(200, {"rows": rows})

        def not_allowed(self):
            self.send_response(405)
            self.send_header("Allow", "GET")
            self.send_header("Content-Length", "0")
            self.end_headers()

        do_POST = do_PUT = do_PATCH = do_DELETE = do_HEAD = do_OPTIONS = not_allowed

        def log_request(self, code="-", size="-"):
            # Apache logs requests; the journal keeps startup and errors.
            pass

    return Handler


def start(host, port, db_path):
    """Serve the API from a daemon thread, which stops with the collector."""
    server = ThreadingHTTPServer((host, port), make_handler(db_path))
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server

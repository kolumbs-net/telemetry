import os
import signal
import socket
import sqlite3
import time

from store import Store, parse_reading, utc_now

UDP_IP = "0.0.0.0"
UDP_PORT = int(os.environ["TELEMETRY_UDP_PORT"])
DB_PATH = os.environ["TELEMETRY_DB"]

# Batch writes to spare the SD card; a power loss loses at most this much.
FLUSH_SECONDS = 60


def flush(store):
    try:
        store.flush()
    except sqlite3.Error as e:
        print(f"Could not store {len(store.pending)} readings, will retry: {e}")


def stop(signum, frame):
    # systemd stops and restarts with SIGTERM: leave through the finally below,
    # so the buffered readings are written first.
    raise SystemExit(0)


signal.signal(signal.SIGTERM, stop)

store = Store(DB_PATH)
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.bind((UDP_IP, UDP_PORT))
# Wake up regularly, so readings are flushed on time even if none arrive.
sock.settimeout(1)

print(f"UDP Telemetry Server listening on {UDP_IP}:{UDP_PORT}, storing in {DB_PATH}")

next_flush = time.monotonic() + FLUSH_SECONDS
try:
    while True:
        try:
            data, addr = sock.recvfrom(1024)
        except TimeoutError:
            pass
        else:
            try:
                probe, mv, ma = parse_reading(data, addr[0])
            except ValueError as e:
                print(f"Ignored packet from {addr[0]}: {e}")
            else:
                store.add(probe, utc_now(), mv, ma)
        if time.monotonic() >= next_flush:
            flush(store)
            next_flush = time.monotonic() + FLUSH_SECONDS
except KeyboardInterrupt:
    pass
finally:
    flush(store)
    store.close()

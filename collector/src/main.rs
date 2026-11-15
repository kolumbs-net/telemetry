mod api;
mod mcp;
mod store;

use std::env;
use std::io::ErrorKind;
use std::net::UdpSocket;
use std::process::exit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use store::{Store, parse_reading, utc_now};

const UDP_IP: &str = "0.0.0.0";
const API_HOST: &str = "127.0.0.1";
// Batch writes to spare the SD card; a power loss loses at most this much.
const FLUSH_EVERY: Duration = Duration::from_secs(60);

fn setting(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| {
        eprintln!("{name} is not set");
        exit(1)
    })
}

fn port(name: &str) -> u16 {
    setting(name).parse().unwrap_or_else(|_| {
        eprintln!("{name} must be a port number");
        exit(1)
    })
}

fn flush(store: &mut Store) {
    if let Err(e) = store.flush() {
        println!("Could not store {} readings, will retry: {e}", store.pending.len());
    }
}

fn fail(what: &str, e: impl std::fmt::Display) -> ! {
    eprintln!("{what}: {e}");
    exit(1)
}

fn main() {
    let udp_port = port("TELEMETRY_UDP_PORT");
    let db_path = setting("TELEMETRY_DB");
    let api_port = port("TELEMETRY_API_PORT");

    // systemd stops and restarts with SIGTERM: leave the loop, so the buffered
    // readings are written first.
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, stop.clone()).unwrap_or_else(|e| fail("Could not handle signals", e));
    }

    let mut store = Store::open(&db_path).unwrap_or_else(|e| fail(&format!("Could not open {db_path}"), e));
    let sock = UdpSocket::bind((UDP_IP, udp_port)).unwrap_or_else(|e| fail("Could not bind the UDP port", e));
    // Wake up regularly, so readings are flushed on time even if none arrive.
    sock.set_read_timeout(Some(Duration::from_secs(1))).unwrap_or_else(|e| fail("Could not set a timeout", e));

    println!("UDP Telemetry Server listening on {UDP_IP}:{udp_port}, storing in {db_path}");

    api::start(API_HOST, api_port, &db_path).unwrap_or_else(|e| fail("Could not start the API", e));
    println!("API listening on http://{API_HOST}:{api_port}");

    let mut buf = [0u8; 1024];
    let mut next_flush = Instant::now() + FLUSH_EVERY;
    while !stop.load(Ordering::Relaxed) {
        match sock.recv_from(&mut buf) {
            Ok((n, addr)) => {
                let sender = addr.ip().to_string();
                match parse_reading(&buf[..n], &sender) {
                    Ok((probe, mv, ma)) => store.add(probe, utc_now(), mv, ma),
                    Err(e) => println!("Ignored packet from {sender}: {e}"),
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
            Err(e) => println!("UDP receive failed: {e}"),
        }
        if Instant::now() >= next_flush {
            flush(&mut store);
            next_flush = Instant::now() + FLUSH_EVERY;
        }
    }
    flush(&mut store);
}

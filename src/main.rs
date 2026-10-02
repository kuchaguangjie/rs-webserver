//! `rs-webserver` executable entry point: a minimal static-file HTTP server.
//!
//! Supported routes:
//! - `GET /`         -> returns `hello.html` from the resources directory
//! - `GET /sleep`    -> sleeps 5 seconds, then returns `hello.html` (to demonstrate
//!   how a blocking request occupies a worker thread)
//! - any other path  -> returns `404.html` from the resources directory
//!
//! How to run: `cargo run -- [config-file-path]`; the config path defaults to
//! `config.yml`. The resources directory, bind address, thread-pool size, and task
//! queue capacity all come from the config file; see [`rs_webserver::config`].
//!
//! When the task queue is full, new connections are **rejected via backpressure** and
//! receive `503 Service Unavailable`, so tasks can't pile up without bound
//! (see [`ThreadPool::execute`]).

use std::{
    fs,
    io::{BufReader, prelude::*},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::Arc,
    thread,
    time::Duration,
};

use rs_webserver::{Config, ThreadPool};

/// Default config-file path used when none is given explicitly.
const DEFAULT_CONFIG_PATH: &str = "config.yml";

/// Sleep duration for `GET /sleep`, used to demonstrate a "slow request" occupying a
/// worker thread.
const SLEEP_DURATION: Duration = Duration::from_secs(5);

/// Routing table: maps a request to a static file on disk.
///
/// The paths are computed once at startup to avoid rebuilding them on every
/// connection; they are shared with the worker threads via [`Arc`] (each connection
/// clones the `Arc` and moves it into its task closure).
struct Routes {
    /// File returned for `GET /`.
    hello: PathBuf,
    /// File returned for any unmatched path.
    not_found: PathBuf,
}

fn main() {
    // 1) Read the config: first CLI argument > `config.yml` > built-in defaults.
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());

    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(e) => {
            // Exit on a bad config: safer than running on with an invalid configuration.
            eprintln!("failed to load config ({config_path}): {e}");
            std::process::exit(1);
        }
    };
    println!("effective config: {config:?}");

    // 2) Resolve the full paths of the two resources up front.
    let routes = Arc::new(Routes {
        hello: config.resources_dir.join("hello.html"),
        not_found: config.resources_dir.join("404.html"),
    });

    // 3) Bind the listen address.
    let listener = match TcpListener::bind(&config.bind_address) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("failed to bind address {}: {e}", config.bind_address);
            std::process::exit(1);
        }
    };
    println!(
        "listening on {}, pool size {}, queue capacity {}, resources dir {}",
        config.bind_address,
        config.pool_size,
        config.max_queue_size,
        config.resources_dir.display()
    );

    // 4) Create the fixed-size thread pool (worker count + bounded queue capacity
    //    both come from the config).
    let pool = ThreadPool::with_queue_capacity(config.pool_size, config.max_queue_size);

    // 5) Main loop: accept connections and submit them to the thread pool.
    //    `incoming()` blocks waiting for new connections; the actual request handling
    //    runs concurrently inside the thread pool.
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            // A single failed connection must not terminate the whole server: log and
            // keep accepting the next one.
            Err(e) => {
                eprintln!("failed to accept connection: {e}");
                continue;
            }
        };

        // The task must take ownership of `stream`; so that we can still reply 503 in
        // place when the queue is full, `try_clone` a second handle for the task here
        // and keep the original as a fallback (both share the same underlying socket,
        // which is closed only once both are dropped).
        let task_stream = match stream.try_clone() {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("failed to clone connection handle: {e}");
                continue;
            }
        };

        // Each connection clones the Arc (bumping the refcount only) and moves it into
        // the task closure; this keeps the closure 'static and avoids reallocating the
        // resource paths.
        let routes = Arc::clone(&routes);
        if let Err(e) = pool.execute(move || {
            handle_connection(task_stream, &routes);
        }) {
            // Backpressure: the task queue is full (or the pool is shutting down).
            // Reject immediately with 503 instead of piling tasks up in memory forever.
            eprintln!("rejecting request ({e}), replying 503");
            write_response(
                &mut stream,
                "HTTP/1.1 503 SERVICE UNAVAILABLE",
                "text/plain; charset=utf-8",
                "503 Service Unavailable: server busy, please retry later.\n",
            );
        }
    }
}

/// Handle a single TCP connection: parse the request line, pick a resource, write the
/// response.
///
/// This deliberately **avoids `unwrap`**: a panic inside a task closure would kill the
/// current worker thread, permanently shrinking the pool by one. So every I/O operation
/// that may fail takes a graceful-degradation path.
fn handle_connection(mut stream: TcpStream, routes: &Routes) {
    // Read only the request line (the first line) for routing; the remaining headers
    // are ignored for now.
    let request_line = {
        let buf_reader = BufReader::new(&stream);
        match buf_reader.lines().next() {
            Some(Ok(line)) => line,
            // Empty request or read error: just close the connection, no panic.
            _ => return,
        }
    };

    // Pick the status line and the file to return based on the request line.
    let (status_line, path) = match request_line.as_str() {
        "GET / HTTP/1.1" => ("HTTP/1.1 200 OK", &routes.hello),
        "GET /sleep HTTP/1.1" => {
            // Deliberately slow endpoint: to observe how a slow request occupies a
            // thread in the pool.
            thread::sleep(SLEEP_DURATION);
            ("HTTP/1.1 200 OK", &routes.hello)
        }
        _ => ("HTTP/1.1 404 NOT FOUND", &routes.not_found),
    };

    // Read the file contents; fall back to 500 on failure instead of panicking.
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) => {
            eprintln!("failed to read resource ({}): {e}", path.display());
            write_response(
                &mut stream,
                "HTTP/1.1 500 INTERNAL SERVER ERROR",
                "text/plain; charset=utf-8",
                "500 Internal Server Error",
            );
            return;
        }
    };

    write_response(
        &mut stream,
        status_line,
        "text/html; charset=utf-8",
        &contents,
    );
}

/// Write a response back in HTTP/1.1 format.
///
/// A failed write (e.g. the client disconnected early) is simply ignored — there is no
/// reason to panic in that case.
fn write_response(stream: &mut TcpStream, status_line: &str, content_type: &str, body: &str) {
    let response = format!(
        "{status_line}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    let _ = stream.write_all(response.as_bytes());
}

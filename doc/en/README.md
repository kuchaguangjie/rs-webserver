**Language / 语言:** English | [简体中文](../../README.md)

# rs-webserver

A minimal static-file HTTP server built with the Rust standard library, featuring a
**fixed-size thread pool + bounded task queue**. It is a cleaned-up version of the final
project from *The Rust Book* (single-threaded server → multithreaded server), extended
with a **config file**, **backpressure (503)**, **detailed comments**, **unit tests**, and
a **Makefile**.

## Features

- Standard library only — **zero third-party dependencies**;
- Producer–consumer / work-queue model: `main` pushes jobs into the queue, and idle
  workers pull and run them;
- A **bounded** task queue (`max_queue_size`, default 10000); when it is full the server
  returns **503** as backpressure, so tasks can't pile up unboundedly and blow up memory;
- Configure pool size, queue capacity, resource directory, and bind address via `config.yml`;
- No panics inside worker threads, so a single bad request can't "kill" a thread and
  permanently shrink the pool.

## Layout

```
rs-webserver/
├── Cargo.toml           # package manifest (no external deps)
├── LICENSE              # full text of the Apache License 2.0
├── config.yml           # runtime configuration
├── Makefile             # handy command wrappers
├── README.md            # 简体中文 (default)
├── doc/
│   └── en/
│       └── README.md    # English docs (this file)
├── resource/
│   └── html/            # static assets
│       ├── hello.html   # returned for GET /
│       └── 404.html     # returned for any unmatched path
└── src/
    ├── main.rs          # binary entry: listen, route, backpressure, write response
    ├── lib.rs           # ThreadPool (bounded-queue thread pool)
    └── config.rs        # config loading (YAML-subset parser)
```

## Build & Run

Requires Rust 1.85+ (this project uses edition 2024; developed on 1.98).

```bash
# Option 1: plain cargo
cargo run                     # reads ./config.yml
cargo run -- path/to/conf.yml # use a specific config file

# Option 2: Makefile
make run
make smoke     # start the server and smoke-test each route
make test      # run unit tests
make clippy    # lints
```

Open in a browser or use curl:

```bash
curl http://127.0.0.1:7878/       # 200 -> hello.html
curl http://127.0.0.1:7878/sleep  # 200 after 5s -> hello.html (demonstrates blocking)
curl http://127.0.0.1:7878/nope   # 404 -> 404.html
```

## Configuration (config.yml)

| Key | Description | Default |
| --- | --- | --- |
| `pool_size` | number of worker threads (positive integer) | `4` |
| `max_queue_size` | max task-queue capacity (positive integer) | `10000` |
| `resources_dir` | static resource directory | `resource/html` |
| `bind_address` | listen address | `127.0.0.1:7878` |

- Every key is optional; omitted keys fall back to defaults, and a **missing config file
  also falls back to defaults**.
- Unknown keys cause a startup error, which helps catch typos.
- The parser only supports flat `key: value` pairs (comments, trailing comments, and
  quoted values); see the docs at the top of `src/config.rs`.

Example:

```yaml
pool_size: 8
max_queue_size: 10000
resources_dir: resource/html
bind_address: 0.0.0.0:8080
```

---

## How It Works

### The big picture: producer–consumer / work queue

This project is **not** async; it uses the classic **thread pool + shared task queue**
(a.k.a. work queue / pull model):

```
                     execute(job)                Arc<Mutex<Receiver>>
  producer (main) ──────────────────> [ bounded task queue ] <───────────────┐
  (accept loop)                        (capacity = max_queue_size)  ▲  ▲  ▲  ▲
                                                                   │  │  │  │
                                                              Worker0 ... WorkerN
```

- It is a single **in-process, in-memory** queue, **not** a broker-style MQ (no
  persistence, no cross-process, no ack/redelivery);
- Multiple `execute` calls = **multiple producers**; workers sharing one receiver =
  **multiple consumers**. Under the hood, std's *MPSC* channel plus
  `Arc<Mutex<Receiver>>` combine to give MPMC behavior;
- There is **no fixed binding** between tasks and threads: whoever becomes free first
  takes the next task.

### Lifecycle of one connection

1. `main` blocks on `listener.incoming()` waiting for a new connection;
2. On accept it `try_clone()`s a handle (the original stays behind as a fallback for
   "reply 503 when the queue is full"), `Arc::clone`s the routes, and builds the task
   closure;
3. It calls `ThreadPool::execute` to **push** the closure onto the queue
   (`Box<dyn FnOnce()>`):
   - enqueued → returns `Ok(())`;
   - queue full → returns `Err(QueueFull)`; `main` immediately replies **503** to the
     client and keeps serving the next connection;
4. An idle worker's `recv()` picks up the task and runs `handle_connection`;
5. `handle_connection` parses the request line → picks a resource → reads the file →
   writes the response.

### Why `Arc<Mutex<Receiver>>`

- `Receiver` isn't `Clone`, but N workers must share one receiver — hence the `Arc`;
- `recv()` needs `&mut self` and workers cannot hold mutable borrows at the same time —
  hence the `Mutex` for interior mutability.

Key detail: the lock is **held only during `recv()`** —

```rust
let message = receiver.lock().unwrap().recv(); // the temporary guard is dropped at the end of this statement
// the task runs afterwards, without holding the lock
```

So at any moment only one worker is blocked in `recv()` (no thundering herd /
serialization), while multiple workers can **run their tasks concurrently** — the lock is
not a throughput bottleneck.

### Bounded queue & backpressure

The queue is created with `mpsc::sync_channel(max_queue_size)` (**bounded**), and
submission uses the non-blocking `try_send`:

- **Max in-flight tasks = `pool_size` + `max_queue_size`**
  (`pool_size` running + `max_queue_size` queued);
- Once the limit is reached, new tasks are rejected immediately → the HTTP layer returns
  `503 Service Unavailable`;
- Why `try_send` instead of a blocking `send`: `main` is a single-threaded accept loop,
  so blocking there would also stop accepting new connections; returning an error lets it
  "reject this one, keep serving the rest".

### Graceful shutdown

When `ThreadPool` is dropped: it drops the sender first → the queue closes → each
worker's `recv()` returns `Err` and its loop exits → the main thread `join`s them one by
one, so no threads or tasks leak.

---

## FAQ (Design Q&A)

### Q1: What happens when all pool threads are busy?

It depends on whether the queue is bounded:

- **This project (bounded queue)**: tasks queue up first; once the queue reaches
  `max_queue_size` with no idle thread, new tasks are **rejected immediately** and the
  HTTP layer returns **503**. The number of in-flight tasks has a hard cap
  (`pool_size + max_queue_size`), so memory doesn't grow without bound.
- With an **unbounded queue** (`mpsc::channel()`): tasks are never rejected, they just
  pile up, showing up as **linearly growing latency + steadily growing memory**, and
  eventually OOM if producers outrun consumers.

So an exhausted pool shouldn't silently pile up — it should push pressure back upstream
via **backpressure**.

### Q2: If thread #3 gets stuck, does it affect thread #4? Why?

**It does not directly affect thread #4 itself.** The reason is that there is **no fixed
binding** between workers and requests: all workers "grab" tasks from the same shared
queue, and which worker gets which task is nondeterministic.

- While worker #3 is stuck, worker #4 still locks, calls `recv()`, and runs other tasks
  just fine — there is no lock dependency between them (the lock is held only briefly
  during `recv()`, not while running a task, so they don't block each other);
- The real effect is that you **lose one unit of concurrency capacity**: available workers
  drop from N to N-1, throughput falls, and the queue fills up more easily (making 503s
  more likely).

Two outcomes worth distinguishing:

1. **Some threads stuck**: the others keep working, just with lower overall throughput;
2. **All threads stuck**: no worker is free to `recv()` new tasks, so **even an ultra-fast
   `GET /` ends up queued behind the slow requests** — the classic **head-of-line
   blocking**, which looks like the whole server "hanging".

So a stuck thread doesn't take a specific other thread down with it, but once enough
threads are stuck (especially all of them), the whole service suffers. Mitigations:
**request timeouts**, **isolation** (put slow work in a separate pool/queue), **bounded
queue + backpressure** (already built in here), or switching to **async I/O** (tokio, etc.).

### Measured results (actually run in this repo)

- **Backpressure**: `pool_size=1, max_queue_size=2`, 6 concurrent `/sleep` requests (each
  occupying a thread for 5s) — capacity = 1 running + 2 queued = 3, and exactly
  **3 returned 200 and 3 returned 503**, after which the service recovered.
- **Head-of-line blocking**: with `pool_size=4`, after 4 concurrent `/sleep` requests
  occupied all threads, a `GET /` sent at that point waited **4.49s** to be served
  (`time_total≈4.491537s`) — showing that fast requests queue up behind slow ones once all
  threads are busy.

## License

This project is licensed under the **[Apache License 2.0](../../LICENSE)**; see
[`LICENSE`](../../LICENSE) for the full text.

If you want to put a notice at the top of a source file, use the template
recommended by Apache:

```text
Copyright 2025 eric

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```

Unless required by applicable law or agreed to in writing, software distributed
under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
CONDITIONS OF ANY KIND, either express or implied.

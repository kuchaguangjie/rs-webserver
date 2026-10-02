//! The library part of `rs-webserver`.
//!
//! It currently exposes two pieces of functionality:
//! - [`ThreadPool`]: a fixed-size thread pool for handling connections concurrently;
//! - [`Config`]: runtime configuration read from `config.yml` (see the [`config`]
//!   module).
//!
//! # Thread-pool implementation notes
//!
//! The classic approach is "**one bounded queue + several consumers**"
//! (producer–consumer / work-queue model):
//!
//! ```text
//!            execute(job)                    Arc<Mutex<Receiver>>
//!  producer (main) ───────────> [ bounded task queue ] <───────────────┐
//!                               (capacity = queue_capacity)  |  |  |  |
//!                                                            v  v  v  v
//!                                                        Worker0 ... WorkerN
//! ```
//!
//! - [`ThreadPool::execute`] **pushes** a task (a boxed closure) onto the queue; idle
//!   workers **pull** the next task themselves — i.e. "whoever is free first takes it",
//!   with no fixed binding between tasks and threads;
//! - Each `Worker` holds the same receiver via `Arc<Mutex<Receiver>>`, looping on
//!   `recv()` to grab tasks;
//! - Multiple workers share one `Receiver`, so a `Mutex` is required to guarantee only
//!   one worker reads the channel at a time; the lock is released right after the task
//!   is taken and before it runs, so the lock never becomes a serialization bottleneck.
//!
//! The queue is **bounded** (`mpsc::sync_channel`, capacity via
//! [`ThreadPool::with_queue_capacity`]): when it is full, [`ThreadPool::execute`]
//! immediately returns [`ThreadPoolError::QueueFull`], leaving the caller to decide how
//! to degrade (this project's HTTP layer replies `503 Service Unavailable`), which
//! prevents tasks from piling up without bound and blowing up memory.
//!
//! For questions like "what happens when the pool is exhausted" or "does one stuck
//! thread affect the others", the behavior of `recv()` and the bounded queue is the key
//! to understanding; the "How It Works" section of `README.md` explains this in detail.

pub mod config;

pub use config::Config;

use std::{
    fmt,
    sync::{Arc, Mutex, mpsc},
    thread,
};

/// Default task-queue capacity: how many tasks may wait in the queue (excluding tasks
/// that are currently running).
pub const DEFAULT_QUEUE_CAPACITY: usize = 10_000;

/// Errors that may occur when submitting a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadPoolError {
    /// The task queue is full (it reached the configured capacity), so the task was
    /// rejected.
    ///
    /// This is a **backpressure** signal: production outpaces consumption, and the
    /// caller should degrade (e.g. return 503, retry later) rather than keep piling up
    /// tasks.
    QueueFull,
    /// The thread pool is shut down (the sender was dropped), so no more tasks can be
    /// submitted.
    Shutdown,
}

impl fmt::Display for ThreadPoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThreadPoolError::QueueFull => write!(f, "task queue is full"),
            ThreadPoolError::Shutdown => write!(f, "thread pool is shut down"),
        }
    }
}

impl std::error::Error for ThreadPoolError {}

/// A **one-shot** task that a worker thread can run.
///
/// `Box<dyn FnOnce() + Send + 'static>` boxes an arbitrary closure into a single type so
/// it can be placed into the same channel (the channel requires all message types to
/// match):
/// - `FnOnce`: the task is executed exactly once;
/// - `Send`: the task must be moved across threads to a worker;
/// - `'static`: the task must not borrow stack data that could expire early (the
///   closure must own its data).
type Job = Box<dyn FnOnce() + Send + 'static>;

/// A fixed-size thread pool.
///
/// Creating it spawns `size` worker threads; tasks are then submitted via
/// [`ThreadPool::execute`]. When the `ThreadPool` is dropped, it closes the task
/// channel and waits for all worker threads to exit.
pub struct ThreadPool {
    /// All worker threads. Held in a `Vec` so they can be joined one by one on `drop`.
    workers: Vec<Worker>,
    /// The task sender.
    ///
    /// Wrapped in an `Option` so it can be `take()`n out and dropped early on `drop`:
    /// once the sender is dropped and no other senders exist, the channel closes and
    /// each worker's `recv()` returns `Err`, exiting its loop.
    ///
    /// Note this is a `SyncSender` (bounded queue) rather than a `Sender` (unbounded
    /// queue), which imposes a capacity limit on the queue.
    sender: Option<mpsc::SyncSender<Job>>,
}

impl ThreadPool {
    /// Create a thread pool with the default queue capacity ([`DEFAULT_QUEUE_CAPACITY`]).
    ///
    /// # Panics
    ///
    /// Panics when `size` is 0 — a pool with no threads cannot run tasks.
    pub fn new(size: usize) -> ThreadPool {
        ThreadPool::with_queue_capacity(size, DEFAULT_QUEUE_CAPACITY)
    }

    /// Create a thread pool with `size` threads and a task-queue capacity of
    /// `queue_capacity`.
    ///
    /// `queue_capacity` is the **maximum number of tasks that may wait in the queue**
    /// (excluding tasks currently running, so the system holds at most
    /// `size + queue_capacity` in-flight tasks at once).
    ///
    /// # Panics
    ///
    /// Panics when `size` or `queue_capacity` is 0 — a pool with no threads or no queue
    /// space cannot work. On the normal path, validation via the
    /// [config](crate::Config) already guarantees both are greater than 0.
    pub fn with_queue_capacity(size: usize, queue_capacity: usize) -> ThreadPool {
        // Precondition: at least 1 thread, or tasks would never be executed.
        assert!(size > 0, "pool size must be greater than 0");
        // Precondition: the queue must hold at least 1 task.
        assert!(queue_capacity > 0, "queue capacity must be greater than 0");

        // Create a **bounded** channel: `send` fails/blocks when the queue is full,
        // providing backpressure. Multiple `execute` calls = multiple producers; all
        // workers share the single receiver = single consumer.
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);

        // `Receiver` isn't `Clone`, so use an `Arc` to let multiple workers share the
        // same receiver; and since `recv()` needs `&mut self` and only one worker may
        // consume at a time, wrap it in a `Mutex` for interior mutability.
        let receiver = Arc::new(Mutex::new(receiver));

        // Pre-allocate the capacity to avoid reallocating as we push.
        let mut workers = Vec::with_capacity(size);

        for id in 0..size {
            // `Arc::clone` only bumps the refcount; it does not clone the underlying
            // `Receiver`.
            workers.push(Worker::new(id, Arc::clone(&receiver)));
        }

        ThreadPool {
            workers,
            sender: Some(sender),
        }
    }

    /// Submit a task to the pool to be run asynchronously by an idle worker.
    ///
    /// Returning `Ok(())` means the task was enqueued (not that it has run); returning
    /// [`ThreadPoolError::QueueFull`] means the queue was full and the task was
    /// rejected; returning [`ThreadPoolError::Shutdown`] means the pool is shut down.
    ///
    /// This uses the non-blocking `try_send` rather than a blocking `send`: this
    /// project's caller is a single-threaded accept loop, and blocking there would also
    /// stop accepting new connections; returning an error lets the caller respond `503`
    /// immediately and keep serving other connections.
    pub fn execute<F>(&self, f: F) -> Result<(), ThreadPoolError>
    where
        F: FnOnce() + Send + 'static,
    {
        // Box the closure into the uniform `Job` type.
        let job = Box::new(f);

        match self.sender.as_ref() {
            Some(sender) => match sender.try_send(job) {
                Ok(()) => Ok(()),
                // Queue full: reject the task and let the caller handle backpressure.
                Err(mpsc::TrySendError::Full(_)) => Err(ThreadPoolError::QueueFull),
                // All receivers are gone (the pool is shutting down).
                Err(mpsc::TrySendError::Disconnected(_)) => Err(ThreadPoolError::Shutdown),
            },
            // The sender was already taken out during `drop`.
            None => Err(ThreadPoolError::Shutdown),
        }
    }
}

/// Gracefully shut down all worker threads when the `ThreadPool` is dropped.
impl Drop for ThreadPool {
    fn drop(&mut self) {
        // 1) Drop the sender to close the channel, so workers still waiting in `recv()`
        //    receive `Err`.
        drop(self.sender.take());

        // 2) Join each thread to make sure it has actually finished, so no tasks or
        //    threads leak.
        for worker in &mut self.workers {
            println!("Shutting down worker {}", worker.id);

            if let Some(thread) = worker.thread.take() {
                thread.join().unwrap();
            }
        }
    }
}

/// A worker thread: holds a `JoinHandle` and loops taking tasks from the channel to run.
struct Worker {
    /// Thread id, used only for logging.
    id: usize,
    /// The thread handle. Wrapped in an `Option` so it can be `take()`n on `drop`
    /// (`join` needs ownership).
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    /// Create a worker thread that continuously receives and runs tasks from the shared
    /// channel.
    fn new(id: usize, receiver: Arc<Mutex<mpsc::Receiver<Job>>>) -> Worker {
        let thread = thread::spawn(move || {
            loop {
                // Lock -> block waiting for a task -> **release the lock immediately**
                // after taking one (the temporary guard is dropped at the end of this
                // statement). So only one worker is reading the channel at any moment,
                // and the lock is not held while a task runs, so it doesn't serialize
                // concurrent execution.
                let message = receiver.lock().unwrap().recv();

                match message {
                    Ok(job) => {
                        println!("Worker {id} got a job; executing.");

                        // Run the task. Note: if this closure panics, the panic
                        // propagates up the current thread and terminates it — the pool
                        // then **permanently loses one worker**. That's why the task's
                        // `handle_connection` deliberately avoids `unwrap`-induced
                        // panics.
                        job();
                    }
                    // The channel is closed (the sender was dropped) or an error
                    // occurred: exit the loop and end the thread.
                    Err(_) => {
                        println!("Worker {id} disconnected; shutting down.");
                        break;
                    }
                }
            }
        });

        Worker {
            id,
            thread: Some(thread),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn executes_all_jobs() {
        let pool = ThreadPool::new(4);
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..16 {
            let counter = Arc::clone(&counter);
            pool.execute(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        }

        // Wait for all tasks to finish (a short sleep simplifies this, avoiding an
        // extra synchronization primitive).
        while counter.load(Ordering::SeqCst) < 16 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert_eq!(counter.load(Ordering::SeqCst), 16);
    }

    #[test]
    #[should_panic]
    fn zero_size_panics() {
        let _ = ThreadPool::new(0);
    }

    #[test]
    #[should_panic]
    fn zero_queue_capacity_panics() {
        let _ = ThreadPool::with_queue_capacity(1, 0);
    }

    /// When the queue is full, it should return [`ThreadPoolError::QueueFull`]
    /// (backpressure).
    ///
    /// Setup: 1 thread + queue capacity 1.
    /// The 1st task occupies the only worker (blocking until released), the 2nd task
    /// fills the single queue slot, and the 3rd task is then rejected because the queue
    /// is full.
    #[test]
    fn rejects_jobs_when_queue_full() {
        let pool = ThreadPool::with_queue_capacity(1, 1);

        // Used to confirm "the worker has started running the 1st task".
        let (started_tx, started_rx) = mpsc::channel::<()>();
        // Used to release the 1st task at the end of the test so it returns the worker.
        let (release_tx, release_rx) = mpsc::channel::<()>();

        pool.execute(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv(); // block, occupying this worker
        })
        .unwrap();

        // Make sure the worker has entered the 1st task (it will not pick another task
        // from the queue now).
        started_rx.recv().unwrap();

        // 2nd task: just fits into the capacity-1 queue -> Ok.
        pool.execute(|| {}).unwrap();

        // 3rd task: queue is full -> QueueFull.
        assert_eq!(pool.execute(|| {}), Err(ThreadPoolError::QueueFull));

        // Release the 1st task so `join` doesn't hang on Drop.
        let _ = release_tx.send(());
    }
}

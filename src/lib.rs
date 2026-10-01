//! `rs-webserver` 的库部分。
//!
//! 目前对外提供两块能力：
//! - [`ThreadPool`]：一个固定大小的线程池，用来并发处理连接；
//! - [`Config`]：从 `config.yml` 读取的运行时配置（见 [`config`] 模块）。
//!
//! # 线程池的实现要点
//!
//! 经典做法是“**一条有界队列 + 若干消费者**”（生产者-消费者 / 工作队列模型）：
//!
//! ```text
//!            execute(job)                    Arc<Mutex<Receiver>>
//!  生产者(main) ───────────> [ 有界任务队列 ] <─────────────────┐
//!                            (容量 = queue_capacity)   |  |  |  |
//!                                                       v  v  v  v
//!                                                   Worker0 ... WorkerN
//! ```
//!
//! - [`ThreadPool::execute`] 把任务（一个装箱的闭包）**推入**队列；空闲 worker
//!   会自己**拉取**下一个任务——即“谁有空谁取”，任务与线程之间没有固定绑定；
//! - 每个 `Worker` 持有同一个接收端的 `Arc<Mutex<Receiver>>`，循环 `recv()` 抢任务；
//! - 多个 worker 共享一个 `Receiver`，因此必须用 `Mutex` 保证同一时刻只有一个
//!   worker 在读通道，取到任务后立刻释放锁再去执行——这样锁不会成为串行化瓶颈。
//!
//! 队列是**有界**的（`mpsc::sync_channel`，容量见 [`ThreadPool::with_queue_capacity`]）：
//! 队列满时 [`ThreadPool::execute`] 会立刻返回 [`ThreadPoolError::QueueFull`]，
//! 由调用方决定如何降级（本项目的 HTTP 层会回一个 `503 Service Unavailable`），
//! 从而避免任务无限堆积导致内存暴涨。
//!
//! 关于“线程池被用满会怎样”“某个线程卡住会不会影响别的线程”等问题，`recv()`
//! 与有界队列的行为是理解关键，`README.md` 的“工作原理”一节有详细说明。

pub mod config;

pub use config::Config;

use std::{
    fmt,
    sync::{Arc, Mutex, mpsc},
    thread,
};

/// 默认的任务队列容量：允许最多这么多任务在队列中排队等待（不含正在执行的任务）。
pub const DEFAULT_QUEUE_CAPACITY: usize = 10_000;

/// 提交任务时可能出现的错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadPoolError {
    /// 任务队列已满（到达配置的容量上限），任务被拒绝。
    ///
    /// 这是一种**背压**信号：表示生产速度超过了消费速度，调用方应降级处理
    /// （例如返回 503、稍后重试），而不是继续堆积任务。
    QueueFull,
    /// 线程池已关闭（发送端被丢弃），无法再提交任务。
    Shutdown,
}

impl fmt::Display for ThreadPoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThreadPoolError::QueueFull => write!(f, "任务队列已满"),
            ThreadPoolError::Shutdown => write!(f, "线程池已关闭"),
        }
    }
}

impl std::error::Error for ThreadPoolError {}

/// 一个可以被工作线程执行的**一次性**任务。
///
/// 用 `Box<dyn FnOnce() + Send + 'static>` 把任意闭包装箱成统一类型，
/// 从而能放进同一个通道（通道要求所有消息类型一致）：
/// - `FnOnce`：任务只会被执行一次；
/// - `Send`：任务需要跨线程移动到工作线程；
/// - `'static`：任务不能借用会提前失效的栈上数据（闭包必须拥有自己的数据）。
type Job = Box<dyn FnOnce() + Send + 'static>;

/// 固定大小的线程池。
///
/// 创建时会启动 `size` 个工作线程，之后通过 [`ThreadPool::execute`] 提交任务。
/// 当 `ThreadPool` 被 drop 时，会关闭任务通道并等待所有工作线程退出。
pub struct ThreadPool {
    /// 所有工作线程。用 `Vec` 持有以便在 `drop` 时逐个 join。
    workers: Vec<Worker>,
    /// 任务发送端。
    ///
    /// 用 `Option` 包裹，是为了能在 `drop` 时 `take()` 出来提前丢弃：
    /// 一旦发送端被丢弃、且没有其它发送端存在，通道就会关闭，
    /// 各 worker 的 `recv()` 会返回 `Err` 从而退出循环。
    ///
    /// 注意这里是 `SyncSender`（有界队列）而非 `Sender`（无界队列），
    /// 从而给队列设定了容量上限。
    sender: Option<mpsc::SyncSender<Job>>,
}

impl ThreadPool {
    /// 使用默认队列容量（[`DEFAULT_QUEUE_CAPACITY`]）创建线程池。
    ///
    /// # Panics
    ///
    /// 当 `size` 为 0 时会 panic——一个没有任何线程的池无法执行任务。
    pub fn new(size: usize) -> ThreadPool {
        ThreadPool::with_queue_capacity(size, DEFAULT_QUEUE_CAPACITY)
    }

    /// 创建一个包含 `size` 个线程、任务队列容量为 `queue_capacity` 的线程池。
    ///
    /// `queue_capacity` 是**队列中最多可排队等待的任务数**（不含正在被线程执行的
    /// 任务，所以系统最多同时持有 `size + queue_capacity` 个在途任务）。
    ///
    /// # Panics
    ///
    /// 当 `size` 或 `queue_capacity` 为 0 时会 panic——没有任何线程或队列空间的池
    /// 都无法工作。正常路径下 [配置](crate::Config) 校验已保证两者都大于 0。
    pub fn with_queue_capacity(size: usize, queue_capacity: usize) -> ThreadPool {
        // 前置条件：至少要有 1 个线程，否则任务永远没人执行。
        assert!(size > 0, "线程池大小必须大于 0");
        // 前置条件：队列至少要能容纳 1 个任务。
        assert!(queue_capacity > 0, "任务队列容量必须大于 0");

        // 创建**有界**通道：send 在队列满时会失败/阻塞，形成背压。
        // 多个 execute 调用 = 多生产者；所有 worker 共享唯一接收端 = 单消费者。
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);

        // Receiver 不能被 clone，因此用 Arc 让多个 worker 共享同一个接收端；
        // 又因为 recv() 需要 &mut self 且同一时刻只能一个 worker 消费，
        // 所以再套一层 Mutex 提供内部可变性。
        let receiver = Arc::new(Mutex::new(receiver));

        // 预先分配容量，避免边 push 边扩容。
        let mut workers = Vec::with_capacity(size);

        for id in 0..size {
            // Arc::clone 只是增加引用计数，并不会克隆底层 Receiver。
            workers.push(Worker::new(id, Arc::clone(&receiver)));
        }

        ThreadPool {
            workers,
            sender: Some(sender),
        }
    }

    /// 提交一个任务到线程池，由某个空闲工作线程异步执行。
    ///
    /// 返回 `Ok(())` 表示任务已入队（不代表已执行）；返回
    /// [`ThreadPoolError::QueueFull`] 表示队列已满、任务被拒绝；
    /// 返回 [`ThreadPoolError::Shutdown`] 表示线程池已关闭。
    ///
    /// 这里使用 `try_send`（非阻塞）而不是阻塞式的 `send`：本项目的调用方是
    /// 单线程的 accept 循环，一旦阻塞就会连带停止接收新连接；返回错误让调用方
    /// 有机会立刻响应 `503` 并继续服务其它连接。
    pub fn execute<F>(&self, f: F) -> Result<(), ThreadPoolError>
    where
        F: FnOnce() + Send + 'static,
    {
        // 把闭包装箱成统一的 Job 类型。
        let job = Box::new(f);

        match self.sender.as_ref() {
            Some(sender) => match sender.try_send(job) {
                Ok(()) => Ok(()),
                // 队列已满：拒绝任务，交由调用方做背压处理。
                Err(mpsc::TrySendError::Full(_)) => Err(ThreadPoolError::QueueFull),
                // 接收端已全部消失（线程池正在关闭）。
                Err(mpsc::TrySendError::Disconnected(_)) => Err(ThreadPoolError::Shutdown),
            },
            // 发送端已在 drop 中被取出。
            None => Err(ThreadPoolError::Shutdown),
        }
    }
}

/// `ThreadPool` 被丢弃时，优雅关闭所有工作线程。
impl Drop for ThreadPool {
    fn drop(&mut self) {
        // 1) 丢弃发送端，关闭通道。这样还在 recv() 等待的 worker 会收到 Err。
        drop(self.sender.take());

        // 2) 逐个 join，等待线程真正结束，确保没有任务/线程被泄漏。
        for worker in &mut self.workers {
            println!("Shutting down worker {}", worker.id);

            if let Some(thread) = worker.thread.take() {
                thread.join().unwrap();
            }
        }
    }
}

/// 工作线程：持有一个 `JoinHandle`，循环从通道取任务并执行。
struct Worker {
    /// 线程编号，仅用于日志。
    id: usize,
    /// 线程句柄。用 `Option` 以便在 `drop` 时 `take()`（配合 `join` 需要所有权）。
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    /// 创建一个工作线程：它会不断从共享通道接收并执行任务。
    fn new(id: usize, receiver: Arc<Mutex<mpsc::Receiver<Job>>>) -> Worker {
        let thread = thread::spawn(move || {
            loop {
                // 加锁 -> 阻塞等待任务 -> 取出后**立即释放锁**（临时守卫在本语句
                // 结束时被 drop）。因此任一时刻只有一个 worker 在读通道，
                // 而执行任务时并不持有锁，锁不会把并发执行串行化。
                let message = receiver.lock().unwrap().recv();

                match message {
                    Ok(job) => {
                        println!("Worker {id} got a job; executing.");

                        // 执行任务。注意：如果这个闭包 panic，panic 会沿着
                        // 当前线程向上传播并终止该线程——线程池会因此**永久
                        // 少一个 worker**。所以任务内部的 `handle_connection`
                        // 刻意避免 unwrap 导致的 panic。
                        job();
                    }
                    // 通道关闭（发送端已 drop）或发生错误：退出循环，线程结束。
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

        // 等待所有任务完成（这里用一个短 sleep 简化，避免引入额外的同步原语）。
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

    /// 队列满时应返回 [`ThreadPoolError::QueueFull`]（背压）。
    ///
    /// 构造：1 个线程 + 队列容量 1。
    /// 第 1 个任务占住唯一的 worker（阻塞直到被释放），
    /// 第 2 个任务填满 1 格队列，第 3 个任务就会因队列满被拒绝。
    #[test]
    fn rejects_jobs_when_queue_full() {
        let pool = ThreadPool::with_queue_capacity(1, 1);

        // 用于确认「worker 已经开始执行第 1 个任务」。
        let (started_tx, started_rx) = mpsc::channel::<()>();
        // 用于在测试结束时释放第 1 个任务，让它归还 worker。
        let (release_tx, release_rx) = mpsc::channel::<()>();

        pool.execute(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv(); // 阻塞，占住这个 worker
        })
        .unwrap();

        // 确保 worker 已进入第 1 个任务（此时它不会再取队列里的任务）。
        started_rx.recv().unwrap();

        // 第 2 个任务：刚好放进容量为 1 的队列 -> Ok。
        pool.execute(|| {}).unwrap();

        // 第 3 个任务：队列已满 -> QueueFull。
        assert_eq!(pool.execute(|| {}), Err(ThreadPoolError::QueueFull));

        // 释放第 1 个任务，避免 Drop 时 join 卡住。
        let _ = release_tx.send(());
    }
}

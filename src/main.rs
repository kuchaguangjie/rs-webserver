//! `rs-webserver` 可执行入口：一个极简的静态文件 HTTP 服务器。
//!
//! 支持的路由：
//! - `GET /`         -> 返回资源目录下的 `hello.html`
//! - `GET /sleep`    -> 先睡眠 5 秒再返回 `hello.html`（用于演示线程池的阻塞行为）
//! - 其它任何路径     -> 返回资源目录下的 `404.html`
//!
//! 运行方式：`cargo run -- [配置文件路径]`，配置文件路径缺省为 `config.yml`。
//! 资源目录、监听地址、线程池大小与任务队列容量均由配置文件决定，详见
//! [`rs_webserver::config`]。
//!
//! 当任务队列已满时，新连接会被**背压拒绝**并返回 `503 Service Unavailable`，
//! 以避免任务无限堆积（见 [`ThreadPool::execute`]）。

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

/// 未显式指定时使用的默认配置文件路径。
const DEFAULT_CONFIG_PATH: &str = "config.yml";

/// `GET /sleep` 的睡眠时长，用于演示“慢请求”占用工作线程的效果。
const SLEEP_DURATION: Duration = Duration::from_secs(5);

/// 路由表：把请求映射到磁盘上的静态文件。
///
/// 在启动时一次性算好路径，避免每个连接都重复拼接；用 [`Arc`] 共享给各工作
/// 线程（每个连接会把 `Arc` 克隆一份移动进任务闭包）。
struct Routes {
    /// `GET /` 返回的文件。
    hello: PathBuf,
    /// 未匹配路径返回的文件。
    not_found: PathBuf,
}

fn main() {
    // 1) 读取配置：命令行第一个参数 > `config.yml` > 内置默认值。
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());

    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(e) => {
            // 配置有误时直接退出，比带着错误配置继续运行更安全。
            eprintln!("加载配置失败（{config_path}）：{e}");
            std::process::exit(1);
        }
    };
    println!("生效配置: {config:?}");

    // 2) 预先解析出两个资源的完整路径。
    let routes = Arc::new(Routes {
        hello: config.resources_dir.join("hello.html"),
        not_found: config.resources_dir.join("404.html"),
    });

    // 3) 绑定监听地址。
    let listener = match TcpListener::bind(&config.bind_address) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("无法绑定地址 {}：{e}", config.bind_address);
            std::process::exit(1);
        }
    };
    println!(
        "监听 {}，线程池大小 {}，队列容量 {}，资源目录 {}",
        config.bind_address,
        config.pool_size,
        config.max_queue_size,
        config.resources_dir.display()
    );

    // 4) 创建固定大小的线程池（工作线程数 + 有界任务队列容量均来自配置）。
    let pool = ThreadPool::with_queue_capacity(config.pool_size, config.max_queue_size);

    // 5) 主循环：接收连接并提交给线程池处理。
    //    `incoming()` 会阻塞等待新连接；具体请求处理在线程池中并发进行。
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            // 单个连接建立失败不应终止整个服务器，记录后继续接收下一个。
            Err(e) => {
                eprintln!("接收连接失败: {e}");
                continue;
            }
        };

        // 任务需要拿走 `stream` 的所有权；为了在“队列已满”时还能就地对客户端回
        // 一个 503，这里先 `try_clone` 出第二份句柄交给任务，原件留作兜底
        // （二者共享同一个底层 socket，关闭需等两份都 drop）。
        let task_stream = match stream.try_clone() {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("复制连接句柄失败: {e}");
                continue;
            }
        };

        // 每个连接克隆一份 Arc（仅增加引用计数），移动进任务闭包；
        // 这样闭包满足 'static，且资源路径无需重复分配。
        let routes = Arc::clone(&routes);
        if let Err(e) = pool.execute(move || {
            handle_connection(task_stream, &routes);
        }) {
            // 背压：任务队列已满（或池正在关闭）。直接拒绝并返回 503，
            // 而不是无限制地把任务堆进内存。
            eprintln!("拒绝请求（{e}），返回 503");
            write_response(
                &mut stream,
                "HTTP/1.1 503 SERVICE UNAVAILABLE",
                "text/plain; charset=utf-8",
                "503 Service Unavailable：服务器繁忙，请稍后重试。\n",
            );
        }
    }
}

/// 处理单个 TCP 连接：解析请求行、选择资源、写回响应。
///
/// 这里刻意**不使用 `unwrap`**：任务闭包内部一旦 panic，会杀死当前工作线程，
/// 使线程池永久少一个线程。因此所有可能失败的 I/O 都走优雅降级路径。
fn handle_connection(mut stream: TcpStream, routes: &Routes) {
    // 只读取请求行（第一行）用于路由判断，其余请求头暂时忽略。
    let request_line = {
        let buf_reader = BufReader::new(&stream);
        match buf_reader.lines().next() {
            Some(Ok(line)) => line,
            // 空请求或读取出错：直接关闭连接，不 panic。
            _ => return,
        }
    };

    // 根据请求行选择状态行与要返回的文件。
    let (status_line, path) = match request_line.as_str() {
        "GET / HTTP/1.1" => ("HTTP/1.1 200 OK", &routes.hello),
        "GET /sleep HTTP/1.1" => {
            // 故意变慢的接口：用于观察慢请求如何占用线程池中的线程。
            thread::sleep(SLEEP_DURATION);
            ("HTTP/1.1 200 OK", &routes.hello)
        }
        _ => ("HTTP/1.1 404 NOT FOUND", &routes.not_found),
    };

    // 读取文件内容；失败时回退到 500，而不是 panic。
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) => {
            eprintln!("读取资源失败（{}）：{e}", path.display());
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

/// 按 HTTP/1.1 格式写回响应。
///
/// 写入失败（例如客户端提前断开）只会被忽略——此时没有必要 panic。
fn write_response(stream: &mut TcpStream, status_line: &str, content_type: &str, body: &str) {
    let response = format!(
        "{status_line}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    let _ = stream.write_all(response.as_bytes());
}

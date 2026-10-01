//! 应用配置模块：从 `config.yml` 读取运行时配置，缺省项回落到默认值。
//!
//! 设计取舍：本项目刻意保持 **零第三方依赖**（与 Rust Book 的服务器示例
//! 一脉相承），因此这里没有引入 `serde` / `serde_yaml`，而是实现了一个精简的
//! **YAML 子集** 解析器。它只处理“顶层 `key: value`”这一种形态，对本项目
//! 的配置需求而言已经足够，同时避免了为了几行配置引入整套序列化框架。
//!
//! 支持的语法：
//!
//! ```yaml
//! # 整行注释
//! pool_size: 4                  # 行尾注释同样支持
//! resources_dir: "resource/html" # 值两侧的引号会被去掉
//! bind_address: 127.0.0.1:7878   # 值里可以包含冒号
//! ```
//!
//! 规则：
//! - 键与值以 **第一个** 冒号 `:` 分隔（所以像 `127.0.0.1:7878` 这样的值没问题）；
//! - 空行、注释行被忽略；
//! - 值两端成对的单/双引号会被剥离；
//! - 出现未知键会直接报错，尽早暴露拼写错误，而不是静默忽略。

use std::fmt;
use std::path::{Path, PathBuf};

/// 默认监听地址。
pub const DEFAULT_BIND_ADDRESS: &str = "127.0.0.1:7878";
/// 默认线程池大小（工作线程数量）。
pub const DEFAULT_POOL_SIZE: usize = 4;
/// 默认任务队列容量（最多可排队等待的任务数）。
pub const DEFAULT_MAX_QUEUE_SIZE: usize = 10_000;
/// 默认静态资源目录。
pub const DEFAULT_RESOURCES_DIR: &str = "resource/html";

/// 服务器运行时配置。
///
/// 通过 [`Config::load`] 或 [`Config::from_str`] 构建；任何未在配置文件中
/// 出现的字段都会取 [`Config::default`] 中的默认值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// TCP 监听地址，例如 `127.0.0.1:7878`。
    pub bind_address: String,
    /// 线程池中的工作线程数量，必须大于 0。
    pub pool_size: usize,
    /// 任务队列容量上限：最多允许多少个任务排队等待，必须大于 0。
    /// 队列满时新任务会被拒绝（HTTP 层返回 503），以此形成背压。
    pub max_queue_size: usize,
    /// 静态资源（HTML 文件）所在目录。
    pub resources_dir: PathBuf,
}

impl Default for Config {
    /// 各字段默认值：
    /// 4 个线程、队列容量 10000、监听 `127.0.0.1:7878`、资源位于 `resource/html`。
    fn default() -> Self {
        Self {
            bind_address: DEFAULT_BIND_ADDRESS.to_string(),
            pool_size: DEFAULT_POOL_SIZE,
            max_queue_size: DEFAULT_MAX_QUEUE_SIZE,
            resources_dir: PathBuf::from(DEFAULT_RESOURCES_DIR),
        }
    }
}

/// 配置加载/解析过程中可能出现的错误。
#[derive(Debug)]
pub enum ConfigError {
    /// 读取配置文件时的 I/O 错误。
    Io(std::io::Error),
    /// 解析某一行时出错（携带行号，便于定位）。
    Parse { line: usize, message: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "读取配置文件失败: {e}"),
            ConfigError::Parse { line, message } => {
                write!(f, "配置文件第 {line} 行有误: {message}")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(e) => Some(e),
            ConfigError::Parse { .. } => None,
        }
    }
}

// 允许 `?` 直接把 io::Error 转成 ConfigError。
impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl Config {
    /// 从指定路径加载配置。
    ///
    /// 若文件 **不存在**，返回默认配置（`Ok`）——这样用户不写配置文件也能直接运行。
    /// 若文件存在但内容有误，则返回 [`ConfigError`]。
    pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => text.parse(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("未找到配置文件 {}，使用默认配置。", path.display());
                Ok(Config::default())
            }
            Err(e) => Err(ConfigError::Io(e)),
        }
    }
}

/// 让 `Config` 支持 `"...".parse::<Config>()` 风格的解析。
impl std::str::FromStr for Config {
    type Err = ConfigError;

    /// 从一段 YAML 子集文本解析配置（便于单元测试，无需真实文件）。
    fn from_str(text: &str) -> Result<Config, ConfigError> {
        // 从默认值出发，逐行覆盖。
        let mut config = Config::default();

        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            // 兼容 Windows 的 CRLF：去掉行尾的 '\r'。
            let line = raw_line.trim_end_matches('\r').trim();

            // 跳过空行与整行注释。
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // 以第一个 ':' 切分键和值。
            let Some((key, raw_value)) = line.split_once(':') else {
                return Err(ConfigError::Parse {
                    line: line_no,
                    message: format!("缺少冒号 ':'，无法解析为 `key: value`：{line:?}"),
                });
            };

            let key = key.trim();
            let value = parse_value(raw_value);

            match key {
                "pool_size" => {
                    config.pool_size = parse_positive_usize(key, &value, line_no)?;
                }
                "max_queue_size" => {
                    config.max_queue_size = parse_positive_usize(key, &value, line_no)?;
                }
                "resources_dir" => {
                    if value.is_empty() {
                        return Err(ConfigError::Parse {
                            line: line_no,
                            message: "resources_dir 不能为空".to_string(),
                        });
                    }
                    config.resources_dir = PathBuf::from(value);
                }
                "bind_address" => {
                    if value.is_empty() {
                        return Err(ConfigError::Parse {
                            line: line_no,
                            message: "bind_address 不能为空".to_string(),
                        });
                    }
                    config.bind_address = value;
                }
                // 未知键直接报错：避免用户以为改了配置其实拼错了键。
                other => {
                    return Err(ConfigError::Parse {
                        line: line_no,
                        message: format!(
                            "未知的配置项 {other:?}（支持: pool_size, max_queue_size, resources_dir, bind_address）"
                        ),
                    });
                }
            }
        }

        Ok(config)
    }
}

/// 解析一个“必须为正整数”的配置值（如 `pool_size`、`max_queue_size`）。
fn parse_positive_usize(key: &str, value: &str, line: usize) -> Result<usize, ConfigError> {
    let n: usize = value.parse().map_err(|_| ConfigError::Parse {
        line,
        message: format!("{key} 必须是非负整数，实际为 {value:?}"),
    })?;
    if n == 0 {
        return Err(ConfigError::Parse {
            line,
            message: format!("{key} 必须大于 0"),
        });
    }
    Ok(n)
}

/// 处理冒号右侧的原始文本：剥离行尾注释、首尾空白以及成对的引号。
fn parse_value(raw: &str) -> String {
    let trimmed = raw.trim();

    // 扫描一遍，记录第一个“引号之外”的 '#' 作为注释起点。
    // 只有位于行首或前面是空白的 '#' 才认定为注释，避免误伤 `a#b` 这类值。
    let mut quote: Option<char> = None;
    let mut comment_start = trimmed.len();
    let mut prev_is_space = true; // 行首视作“前置空白”
    for (idx, ch) in trimmed.char_indices() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                }
            }
            None => match ch {
                '\'' | '"' => quote = Some(ch),
                '#' if prev_is_space => {
                    comment_start = idx;
                    break;
                }
                _ => {}
            },
        }
        prev_is_space = ch.is_whitespace();
    }

    let value = trimmed[..comment_start].trim();

    // 去掉成对的引号（"'x'" 与 "\"x\""），不匹配时保持原样。
    let unquoted = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value);

    unquoted.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn defaults_when_empty() {
        let c = Config::from_str("").unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn overrides_values() {
        let c =
            Config::from_str("pool_size: 8\nresources_dir: \"public\"\nbind_address: 0.0.0.0:80\n")
                .unwrap();
        assert_eq!(c.pool_size, 8);
        assert_eq!(c.resources_dir, PathBuf::from("public"));
        assert_eq!(c.bind_address, "0.0.0.0:80");
    }

    #[test]
    fn default_queue_size_is_10k() {
        assert_eq!(Config::default().max_queue_size, 10_000);
        assert_eq!(Config::from_str("").unwrap().max_queue_size, 10_000);
    }

    #[test]
    fn overrides_queue_size() {
        let c = Config::from_str("max_queue_size: 256\n").unwrap();
        assert_eq!(c.max_queue_size, 256);
    }

    #[test]
    fn rejects_zero_queue_size() {
        assert!(Config::from_str("max_queue_size: 0\n").is_err());
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let c = Config::from_str("# top comment\n\npool_size: 2 # inline\n").unwrap();
        assert_eq!(c.pool_size, 2);
    }

    #[test]
    fn keeps_colon_in_value() {
        let c = Config::from_str("bind_address: 127.0.0.1:7878\n").unwrap();
        assert_eq!(c.bind_address, "127.0.0.1:7878");
    }

    #[test]
    fn rejects_zero_pool_size() {
        assert!(Config::from_str("pool_size: 0\n").is_err());
    }

    #[test]
    fn rejects_unknown_key() {
        assert!(Config::from_str("poolsize: 4\n").is_err());
    }

    #[test]
    fn rejects_non_numeric_pool_size() {
        assert!(Config::from_str("pool_size: four\n").is_err());
    }
}

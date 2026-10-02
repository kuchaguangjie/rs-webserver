//! Application config module: reads runtime config from `config.yml`, falling back to
//! defaults for omitted keys.
//!
//! Design trade-off: this project deliberately stays **free of third-party
//! dependencies** (in keeping with the Rust Book's server example), so instead of
//! pulling in `serde` / `serde_yaml`, it implements a small **YAML-subset** parser. It
//! handles only the "top-level `key: value`" shape, which is enough for this project's
//! configuration needs while avoiding a full serialization framework for a few lines of
//! config.
//!
//! Supported syntax:
//!
//! ```yaml
//! # whole-line comment
//! pool_size: 4                  # trailing comments are supported too
//! resources_dir: "resource/html" # quotes around the value are stripped
//! bind_address: 127.0.0.1:7878   # the value may contain colons
//! ```
//!
//! Rules:
//! - A key and value are separated by the **first** colon `:`, so values like
//!   `127.0.0.1:7878` are fine;
//! - Blank lines and comment lines are ignored;
//! - Matching single/double quotes around a value are stripped;
//! - An unknown key is an immediate error, surfacing typos early instead of silently
//!   ignoring them.

use std::fmt;
use std::path::{Path, PathBuf};

/// Default bind address.
pub const DEFAULT_BIND_ADDRESS: &str = "127.0.0.1:7878";
/// Default thread-pool size (number of worker threads).
pub const DEFAULT_POOL_SIZE: usize = 4;
/// Default task-queue capacity (max number of tasks that may wait in the queue).
pub const DEFAULT_MAX_QUEUE_SIZE: usize = 10_000;
/// Default static resources directory.
pub const DEFAULT_RESOURCES_DIR: &str = "resource/html";

/// Server runtime configuration.
///
/// Built via [`Config::load`] or [`Config::from_str`]; any field absent from the config
/// file takes the value from [`Config::default`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// TCP listen address, e.g. `127.0.0.1:7878`.
    pub bind_address: String,
    /// Number of worker threads in the pool; must be greater than 0.
    pub pool_size: usize,
    /// Task-queue capacity limit: the max number of tasks allowed to wait in the queue;
    /// must be greater than 0. When the queue is full, new tasks are rejected (the HTTP
    /// layer returns 503), providing backpressure.
    pub max_queue_size: usize,
    /// Directory containing the static resources (HTML files).
    pub resources_dir: PathBuf,
}

impl Default for Config {
    /// Per-field defaults:
    /// 4 threads, queue capacity 10000, listening on `127.0.0.1:7878`, resources in
    /// `resource/html`.
    fn default() -> Self {
        Self {
            bind_address: DEFAULT_BIND_ADDRESS.to_string(),
            pool_size: DEFAULT_POOL_SIZE,
            max_queue_size: DEFAULT_MAX_QUEUE_SIZE,
            resources_dir: PathBuf::from(DEFAULT_RESOURCES_DIR),
        }
    }
}

/// Errors that may occur while loading/parsing the config.
#[derive(Debug)]
pub enum ConfigError {
    /// An I/O error while reading the config file.
    Io(std::io::Error),
    /// An error parsing a particular line (carries the line number for easy location).
    Parse { line: usize, message: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "failed to read config file: {e}"),
            ConfigError::Parse { line, message } => {
                write!(f, "config file line {line} is invalid: {message}")
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

// Lets `?` convert an `io::Error` straight into a `ConfigError`.
impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl Config {
    /// Load the config from the given path.
    ///
    /// If the file **does not exist**, returns the default config (`Ok`) — so users can
    /// run without writing a config file. If the file exists but its contents are
    /// invalid, returns a [`ConfigError`].
    pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => text.parse(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("config file {} not found, using defaults.", path.display());
                Ok(Config::default())
            }
            Err(e) => Err(ConfigError::Io(e)),
        }
    }
}

/// Lets `Config` support `"...".parse::<Config>()`-style parsing.
impl std::str::FromStr for Config {
    type Err = ConfigError;

    /// Parse the config from a YAML-subset string (handy for unit tests, no real file
    /// needed).
    fn from_str(text: &str) -> Result<Config, ConfigError> {
        // Start from the defaults and override line by line.
        let mut config = Config::default();

        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            // Handle Windows CRLF: strip a trailing '\r'.
            let line = raw_line.trim_end_matches('\r').trim();

            // Skip blank lines and whole-line comments.
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Split the key and value on the first ':'.
            let Some((key, raw_value)) = line.split_once(':') else {
                return Err(ConfigError::Parse {
                    line: line_no,
                    message: format!("missing ':' separator, cannot parse as `key: value`: {line:?}"),
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
                            message: "resources_dir must not be empty".to_string(),
                        });
                    }
                    config.resources_dir = PathBuf::from(value);
                }
                "bind_address" => {
                    if value.is_empty() {
                        return Err(ConfigError::Parse {
                            line: line_no,
                            message: "bind_address must not be empty".to_string(),
                        });
                    }
                    config.bind_address = value;
                }
                // Unknown keys are an immediate error: so users don't think they changed
                // the config when they actually misspelled a key.
                other => {
                    return Err(ConfigError::Parse {
                        line: line_no,
                        message: format!(
                            "unknown config key {other:?} (supported: pool_size, max_queue_size, resources_dir, bind_address)"
                        ),
                    });
                }
            }
        }

        Ok(config)
    }
}

/// Parse a config value that "must be a positive integer" (such as `pool_size`,
/// `max_queue_size`).
fn parse_positive_usize(key: &str, value: &str, line: usize) -> Result<usize, ConfigError> {
    let n: usize = value.parse().map_err(|_| ConfigError::Parse {
        line,
        message: format!("{key} must be a non-negative integer, got {value:?}"),
    })?;
    if n == 0 {
        return Err(ConfigError::Parse {
            line,
            message: format!("{key} must be greater than 0"),
        });
    }
    Ok(n)
}

/// Process the raw text to the right of the colon: strip a trailing comment, surrounding
/// whitespace, and matching quotes.
fn parse_value(raw: &str) -> String {
    let trimmed = raw.trim();

    // Scan once, recording the first '#' *outside quotes* as the start of a comment.
    // Only a '#' at the start of the line or preceded by whitespace counts as a comment,
    // to avoid mangling values like `a#b`.
    let mut quote: Option<char> = None;
    let mut comment_start = trimmed.len();
    let mut prev_is_space = true; // the line start counts as "preceded by whitespace"
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

    // Strip matching quotes ("'x'" and "\"x\""); leave as-is if they don't match.
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

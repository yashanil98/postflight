use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub session_retention: usize,
    pub workspace_root: Option<PathBuf>,
    pub exclude_patterns: Vec<String>,
    pub network_poll_interval_ms: u64,
    pub process_poll_interval_ms: u64,
    pub max_duration_secs: Option<u64>,
    pub grace_period_secs: u64,
    pub shutdown_message: String,
}

const BUILTIN_EXCLUDE_PATTERNS: &[&str] = &[
    ".git/**",
    ".postflight/**",
];

impl Default for Config {
    fn default() -> Self {
        Self {
            session_retention: 20,
            workspace_root: None,
            exclude_patterns: vec![
                ".git/**".to_string(),
                "target/**".to_string(),
                "node_modules/**".to_string(),
                ".postflight/**".to_string(),
            ],
            network_poll_interval_ms: 500,
            process_poll_interval_ms: 250,
            max_duration_secs: None,
            grace_period_secs: 60,
            shutdown_message: "You have 60 seconds to finish. Wrap up and produce your final output now.\n".to_string(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path();
        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)?;
            match toml::from_str(&content) {
                Ok(config) => Ok(config),
                Err(e) => {
                    eprintln!(
                        "warning: failed to parse {}, using defaults: {e}",
                        config_path.display()
                    );
                    Ok(Self::default())
                }
            }
        } else {
            Ok(Self::default())
        }
    }

    pub fn config_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".postflight")
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    pub fn sessions_dir() -> PathBuf {
        Self::config_dir().join("sessions")
    }

    pub fn should_exclude(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        let mut all_patterns = self
            .exclude_patterns
            .iter()
            .map(String::as_str)
            .chain(BUILTIN_EXCLUDE_PATTERNS.iter().copied());
        all_patterns.any(|pattern| {
            if let Some(prefix) = pattern.strip_suffix("/**") {
                let sep_prefix_sep = format!("/{prefix}/");
                let prefix_sep = format!("{prefix}/");
                let sep_prefix = format!("/{prefix}");
                path_str.contains(&sep_prefix_sep)
                    || path_str.starts_with(&prefix_sep)
                    || path_str.ends_with(&sep_prefix)
                    || *path_str == *prefix
            } else {
                glob_match(pattern, &path_str)
            }
        })
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('*').collect();
    if pattern_parts.len() == 1 {
        return text == pattern;
    }

    let mut pos = 0;
    for (i, part) in pattern_parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if let Some(found) = text[pos..].find(part) {
            if i == 0 && found != 0 {
                return false;
            }
            pos += found + part.len();
        } else {
            return false;
        }
    }

    if let Some(last) = pattern_parts.last() {
        if !last.is_empty() {
            return text.ends_with(last);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.session_retention, 20);
        assert_eq!(config.exclude_patterns.len(), 4);
    }

    #[test]
    fn test_should_exclude() {
        let config = Config::default();
        assert!(config.should_exclude(Path::new("project/.git/objects/abc")));
        assert!(config.should_exclude(Path::new("project/target/debug/build")));
        assert!(config.should_exclude(Path::new("project/node_modules/foo")));
        assert!(!config.should_exclude(Path::new("project/src/main.rs")));
    }

    #[test]
    fn test_should_exclude_respects_path_boundaries() {
        let config = Config::default();
        assert!(!config.should_exclude(Path::new("project/.gitignore")));
        assert!(!config.should_exclude(Path::new("project/retarget/foo.rs")));
        assert!(!config.should_exclude(Path::new("project/src/node_modules_helper.rs")));
        assert!(config.should_exclude(Path::new(".git/HEAD")));
        assert!(config.should_exclude(Path::new("target/release/bin")));
    }

    #[test]
    fn test_should_exclude_builtin_patterns_always_apply() {
        let config = Config {
            exclude_patterns: vec!["*.log".to_string()],
            ..Config::default()
        };
        assert!(config.should_exclude(Path::new("project/.git/objects/abc")));
        assert!(config.should_exclude(Path::new("project/.postflight/sessions/x")));
        assert!(config.should_exclude(Path::new("debug.log")));
        assert!(!config.should_exclude(Path::new("project/src/main.rs")));
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.py"));
        assert!(glob_match("src/*", "src/main.rs"));
    }
}

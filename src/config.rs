use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs::{self, Permissions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

// Reuse gmail-cli's Google Cloud project OAuth credentials
// Just needs YouTube Data API v3 enabled on the same project
pub const DEFAULT_CLIENT_ID: &str =
    "690797697044-6kpkd2ethnsren8m5v27qdkj2182eb4n.apps.googleusercontent.com";
pub const DEFAULT_CLIENT_SECRET: &str = "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";

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Config {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

impl Config {
    pub fn client_id(&self) -> &str {
        self.client_id.as_deref().unwrap_or(DEFAULT_CLIENT_ID)
    }
    pub fn client_secret(&self) -> &str {
        self.client_secret
            .as_deref()
            .unwrap_or(DEFAULT_CLIENT_SECRET)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("youtube-cli")
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn tokens_path() -> PathBuf {
    config_dir().join("tokens.json")
}

fn write_secure(path: &PathBuf, content: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

pub fn load_config() -> Result<Config> {
    let path = config_path();
    if path.exists() {
        let content = fs::read_to_string(&path)?;
        return Ok(serde_json::from_str(&content)?);
    }
    Ok(Config::default())
}

fn ensure_config_dir() -> Result<PathBuf> {
    let dir = config_dir();
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
        fs::set_permissions(&dir, Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

pub fn save_config(config: &Config) -> Result<()> {
    ensure_config_dir()?;
    write_secure(&config_path(), &serde_json::to_string_pretty(config)?)
}

pub fn load_tokens() -> Result<Tokens> {
    let path = tokens_path();
    let content = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&content)?)
}

pub fn save_tokens(tokens: &Tokens) -> Result<()> {
    ensure_config_dir()?;
    write_secure(&tokens_path(), &serde_json::to_string_pretty(tokens)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct XdgGuard {
        previous: Option<String>,
    }

    impl Drop for XdgGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var("XDG_CONFIG_HOME", previous);
                } else {
                    std::env::remove_var("XDG_CONFIG_HOME");
                }
            }
        }
    }

    fn with_config_home(test: impl FnOnce(&std::path::Path)) {
        let _lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let path = std::env::temp_dir().join(format!(
            "youtube-config-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        let guard = XdgGuard {
            previous: std::env::var("XDG_CONFIG_HOME").ok(),
        };
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &path);
        }

        test(&path);

        drop(guard);
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn default_config_uses_bundled_oauth_credentials() {
        let config = Config::default();

        assert_eq!(config.client_id(), DEFAULT_CLIENT_ID);
        assert_eq!(config.client_secret(), DEFAULT_CLIENT_SECRET);
    }

    #[test]
    fn custom_config_overrides_defaults() {
        let config = Config {
            client_id: Some("id".to_string()),
            client_secret: Some("secret".to_string()),
        };

        assert_eq!(config.client_id(), "id");
        assert_eq!(config.client_secret(), "secret");
    }

    #[test]
    fn saves_and_loads_config_and_tokens_under_xdg_home() {
        with_config_home(|root| {
            let config = Config {
                client_id: Some("id".to_string()),
                client_secret: Some("secret".to_string()),
            };
            let tokens = Tokens {
                access_token: "access".to_string(),
                refresh_token: "refresh".to_string(),
            };

            save_config(&config).unwrap();
            save_tokens(&tokens).unwrap();

            assert_eq!(config_dir(), root.join("youtube-cli"));
            assert_eq!(config_path(), root.join("youtube-cli/config.json"));
            assert_eq!(tokens_path(), root.join("youtube-cli/tokens.json"));
            let loaded_config = load_config().unwrap();
            let loaded_tokens = load_tokens().unwrap();
            assert_eq!(loaded_config.client_id.as_deref(), Some("id"));
            assert_eq!(loaded_tokens.access_token, "access");
            assert_eq!(loaded_tokens.refresh_token, "refresh");
            assert_eq!(
                std::fs::metadata(config_dir())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(tokens_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        });
    }

    #[test]
    fn load_config_returns_default_when_missing_and_errors_on_invalid_json() {
        with_config_home(|_| {
            assert!(load_config().unwrap().client_id.is_none());
            std::fs::create_dir_all(config_dir()).unwrap();
            std::fs::write(config_path(), "not-json").unwrap();

            let err = load_config().unwrap_err();

            assert!(err.to_string().contains("expected ident"));
        });
    }
}

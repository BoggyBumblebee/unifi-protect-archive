use std::{env, fs, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMethod {
    Auto,
    ApiKey,
    Password,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub controller: String,
    pub auth_method: AuthMethod,
    pub api_key_env: String,
    pub username_env: String,
    pub password_env: String,
    pub archive_destination: String,
    pub archive_host: String,
    pub archive_shared_drive: String,
    pub camera_ids: Vec<String>,
    pub segment_seconds: u64,
    pub lookback_seconds: u64,
    pub minimum_age_seconds: u64,
    pub poll_seconds: u64,
    pub archive_status_poll_seconds: u64,
    pub wait_for_archive_completion: bool,
    pub delete_after_archive: bool,
    pub delete_after_archive_confirmation: String,
    pub verify_tls: bool,
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            controller: "https://unifi-console.example.invalid".to_string(),
            auth_method: AuthMethod::Auto,
            api_key_env: "UNIFI_PROTECT_API_KEY".to_string(),
            username_env: "UNIFI_PROTECT_USERNAME".to_string(),
            password_env: "UNIFI_PROTECT_PASSWORD".to_string(),
            archive_destination: "NAS".to_string(),
            archive_host: "nas.example.invalid".to_string(),
            archive_shared_drive: "ProtectArchive".to_string(),
            camera_ids: Vec::new(),
            segment_seconds: 15 * 60,
            lookback_seconds: 60 * 60,
            minimum_age_seconds: 2 * 60,
            poll_seconds: 5 * 60,
            archive_status_poll_seconds: 15,
            wait_for_archive_completion: true,
            delete_after_archive: false,
            delete_after_archive_confirmation: String::new(),
            verify_tls: true,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("failed to parse config file {}", path.display()))
    }

    pub fn sample() -> Result<String> {
        let sample = Self {
            controller: "https://unifi-console.example.invalid".to_string(),
            verify_tls: false,
            camera_ids: vec![],
            archive_host: "nas.example.invalid".to_string(),
            archive_shared_drive: "ProtectArchive".to_string(),
            ..Self::default()
        };
        toml::to_string_pretty(&sample).context("failed to render sample config")
    }

    pub fn api_key(&self) -> Option<String> {
        env::var(&self.api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
    }

    pub fn credentials(&self) -> Result<Credentials> {
        let username = env::var(&self.username_env)
            .with_context(|| format!("set ${} or ${}", self.api_key_env, self.username_env))?;
        let password = env::var(&self.password_env)
            .with_context(|| format!("set ${} or ${}", self.api_key_env, self.password_env))?;

        Ok(Credentials { username, password })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn sample_config_round_trips() {
        let sample = Config::sample().unwrap();

        let config = toml::from_str::<Config>(&sample).unwrap();

        assert_eq!(config.controller, "https://unifi-console.example.invalid");
        assert_eq!(config.auth_method, AuthMethod::Auto);
        assert!(!config.verify_tls);
        assert!(!config.delete_after_archive);
        assert!(config.delete_after_archive_confirmation.is_empty());
    }

    #[test]
    fn load_applies_defaults_for_missing_fields() {
        let path = temp_config_path();
        fs::write(
            &path,
            r#"
controller = "https://example.invalid"
archive_destination = "LOCAL"
"#,
        )
        .unwrap();

        let config = Config::load(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(config.controller, "https://example.invalid");
        assert_eq!(config.auth_method, AuthMethod::Auto);
        assert_eq!(config.segment_seconds, 900);
        assert_eq!(config.archive_host, "nas.example.invalid");
        assert!(!config.delete_after_archive);
    }

    #[test]
    fn load_reports_toml_parse_errors() {
        let path = temp_config_path();
        fs::write(&path, "camera_ids = [not quoted]").unwrap();

        let error = Config::load(&path).unwrap_err();
        fs::remove_file(&path).unwrap();

        assert!(error.to_string().contains("failed to parse config file"));
    }

    #[test]
    fn load_reports_missing_files() {
        let path = temp_config_path();

        let error = Config::load(&path).unwrap_err();

        assert!(error.to_string().contains("failed to read config file"));
    }

    #[test]
    fn api_key_ignores_missing_or_blank_values() {
        let missing_env = unique_env_name("missing-api-key");
        let blank_env = unique_env_name("blank-api-key");
        env::set_var(&blank_env, "   ");

        let missing_config = Config {
            api_key_env: missing_env,
            ..Config::default()
        };
        let blank_config = Config {
            api_key_env: blank_env.clone(),
            ..Config::default()
        };

        assert_eq!(missing_config.api_key(), None);
        assert_eq!(blank_config.api_key(), None);

        env::remove_var(blank_env);
    }

    #[test]
    fn api_key_reads_configured_environment_variable() {
        let api_key_env = unique_env_name("api-key");
        env::set_var(&api_key_env, "test-api-key");
        let config = Config {
            api_key_env: api_key_env.clone(),
            ..Config::default()
        };

        assert_eq!(config.api_key().as_deref(), Some("test-api-key"));

        env::remove_var(api_key_env);
    }

    #[test]
    fn credentials_read_configured_environment_variables() {
        let username_env = unique_env_name("username");
        let password_env = unique_env_name("password");
        env::set_var(&username_env, "service-user");
        env::set_var(&password_env, "service-password");
        let config = Config {
            username_env: username_env.clone(),
            password_env: password_env.clone(),
            ..Config::default()
        };

        let credentials = config.credentials().unwrap();

        assert_eq!(credentials.username, "service-user");
        assert_eq!(credentials.password, "service-password");

        env::remove_var(username_env);
        env::remove_var(password_env);
    }

    #[test]
    fn credentials_error_mentions_api_key_or_missing_username() {
        let username_env = unique_env_name("username-missing");
        let password_env = unique_env_name("password-unused");
        let config = Config {
            api_key_env: "TEST_API_KEY_ENV".to_string(),
            username_env,
            password_env,
            ..Config::default()
        };

        let error = config.credentials().unwrap_err();

        assert!(error.to_string().contains("TEST_API_KEY_ENV"));
    }

    fn temp_config_path() -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!(
            "unifi-protect-archive-config-{unique}-{}.toml",
            std::process::id()
        ))
    }

    fn unique_env_name(label: &str) -> String {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!(
            "UNIFI_PROTECT_ARCHIVE_TEST_{}_{}_{}",
            label.replace('-', "_").to_uppercase(),
            std::process::id(),
            unique
        )
    }
}

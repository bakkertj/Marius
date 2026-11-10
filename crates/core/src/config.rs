//! Where Marius keeps its own state, and the settings the user controls.
//!
//! Requirements: KB-008 (independent vault-level settings), SYNC-001 (works
//! with no account and no network), SEC-001 (**credentials never live in
//! application settings**), AI-003, AI-016.
//!
//! The database and this file hold no secrets. API keys belong in the platform
//! credential store, and the configuration refers to them by account name only
//! — so a settings file can be copied, diffed, backed up or pasted into a bug
//! report without leaking anything.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Marius's own directory, by platform convention.
///
/// Nothing in it is required for the user's knowledge to survive: the vault
/// directories are authoritative, and everything here is either rebuildable or
/// replaceable (integrity rule 3). Deleting it loses preferences and derived
/// indexes, never notes.
pub fn default_data_dir() -> PathBuf {
    if let Ok(explicit) = std::env::var("MARIUS_DATA_DIR") {
        return PathBuf::from(explicit);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Marius")
    } else if cfg!(target_os = "windows") {
        std::env::var("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join("Marius"))
            .unwrap_or_else(|_| home.join("AppData/Roaming/Marius"))
    } else {
        std::env::var("XDG_DATA_HOME")
            .map(|xdg| PathBuf::from(xdg).join("marius"))
            .unwrap_or_else(|_| home.join(".local/share/marius"))
    }
}

/// A model to use for one task (AI-003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    /// Provider identifier, e.g. `ollama`, `openrouter`, `openai-compatible`.
    pub provider: String,
    pub model: String,
    /// Name of the entry in the platform credential store that holds this
    /// provider's key. **Never the key itself** (SEC-001).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// Base URL for a self-hosted or OpenAI-compatible endpoint (AI-001).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Where this endpoint runs, for the policy engine (AI-005). Omitted,
    /// it is inferred: `ollama` is local, anything else is remote — the
    /// conservative reading. Set `self_hosted` for your own server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locality: Option<marius_model::Locality>,
    /// The user trusts this non-local provider with `Personal` material
    /// (AI-006). Meaningless for local providers; defaults to not trusted.
    #[serde(default)]
    pub trusted_for_personal: bool,
}

impl ModelChoice {
    /// The provider as the policy engine sees it.
    pub fn profile(&self) -> marius_llm_policy::ProviderProfile {
        let locality = self.locality.unwrap_or(if self.provider == "ollama" {
            marius_model::Locality::Local
        } else {
            marius_model::Locality::Remote
        });
        marius_llm_policy::ProviderProfile {
            name: self.provider.clone(),
            model: self.model.clone(),
            locality,
            trusted_for_personal: self.trusted_for_personal
                || locality == marius_model::Locality::Local,
        }
    }
}

/// Which model answers which question (AI-003).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagging: Option<ModelChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<ModelChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarization: Option<ModelChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_answering: Option<ModelChoice>,
}

/// User preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Schema version of this file, so an older build can refuse rather than
    /// misread a newer one.
    pub version: u32,
    /// Vault opened at startup, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_vault: Option<String>,
    #[serde(default)]
    pub models: ModelSettings,
    /// Resolve `[[links]]` against every open vault, not just the current one
    /// (GRAPH-008). Off by default: a link should mean what the vault it lives
    /// in says it means.
    #[serde(default)]
    pub cross_vault_links: bool,
    /// Produce passages for semantic search during indexing. Off until an
    /// embedding model is configured, because chunking without one is wasted
    /// work (SRCH-007).
    #[serde(default)]
    pub chunk_on_index: bool,
    #[serde(default = "default_true")]
    pub record_revisions: bool,
    /// Largest revision body to keep, in KiB. `0` records revision metadata
    /// without any content.
    ///
    /// Keeping bodies is what makes "recover an earlier revision" (KB-011)
    /// work for edits made *outside* Marius: once another editor has
    /// overwritten the file, the previous bytes exist nowhere else. The price
    /// is roughly one extra copy of the vault on disk, which is why it is a
    /// setting rather than a silent cost — but turning it off genuinely gives
    /// up recovery, it does not merely economise.
    #[serde(default = "default_revision_body_kib")]
    pub revision_body_limit_kib: u64,
    /// Days of acknowledged journal history to keep. `0` keeps everything.
    ///
    /// The journal is the replication queue and the audit trail (SEC-006).
    /// Entries that have been acknowledged — which, for a vault with no sync
    /// backend, means every entry — have no replication value left, so a
    /// window bounds what is otherwise unbounded growth: a single
    /// 10,000-document index writes about 73,000 rows.
    #[serde(default = "default_journal_retention_days")]
    pub journal_retention_days: u32,
}

fn default_revision_body_kib() -> u64 {
    256
}

fn default_journal_retention_days() -> u32 {
    30
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            default_vault: None,
            models: ModelSettings::default(),
            cross_vault_links: false,
            chunk_on_index: false,
            record_revisions: true,
            revision_body_limit_kib: default_revision_body_kib(),
            journal_retention_days: default_journal_retention_days(),
        }
    }
}

/// Failure reading or writing configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read configuration at {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot write configuration at {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("configuration at {path} is not valid: {source}")]
    Invalid {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(
        "configuration at {path} is version {found}, newer than this build supports ({supported})"
    )]
    TooNew {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
}

impl Config {
    /// Version of the configuration schema.
    pub const VERSION: u32 = 1;

    /// Filename inside the data directory.
    pub const FILE_NAME: &'static str = "config.json";

    /// Load configuration, returning the default when the file is absent.
    ///
    /// A missing file is not an error: Marius must start and be useful with no
    /// setup at all (SYNC-001).
    pub fn load(data_dir: &Path) -> Result<Self, ConfigError> {
        let path = data_dir.join(Self::FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        let config: Self = serde_json::from_str(&text).map_err(|source| ConfigError::Invalid {
            path: path.clone(),
            source,
        })?;
        if config.version > Self::VERSION {
            return Err(ConfigError::TooNew {
                path,
                found: config.version,
                supported: Self::VERSION,
            });
        }
        Ok(config)
    }

    /// Write configuration, creating the directory if needed.
    pub fn save(&self, data_dir: &Path) -> Result<(), ConfigError> {
        std::fs::create_dir_all(data_dir).map_err(|source| ConfigError::Write {
            path: data_dir.to_path_buf(),
            source,
        })?;
        let path = data_dir.join(Self::FILE_NAME);
        let text = serde_json::to_string_pretty(self).map_err(|source| ConfigError::Invalid {
            path: path.clone(),
            source,
        })?;
        // The same temp-then-rename discipline the notes get: a crash during a
        // settings write must not leave an unparseable file that stops the
        // application starting.
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, format!("{text}\n")).map_err(|source| ConfigError::Write {
            path: temp.clone(),
            source,
        })?;
        std::fs::rename(&temp, &path).map_err(|source| ConfigError::Write { path, source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_and_body_limits_round_trip_and_can_be_switched_off() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            revision_body_limit_kib: 0,
            journal_retention_days: 0,
            ..Config::default()
        };
        config.save(dir.path()).unwrap();
        let loaded = Config::load(dir.path()).unwrap();
        assert_eq!(loaded.revision_body_limit_kib, 0);
        assert_eq!(loaded.journal_retention_days, 0);
    }

    #[test]
    fn a_missing_file_yields_defaults_so_the_app_starts_with_no_setup() {
        // SYNC-001.
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(dir.path()).unwrap();
        assert_eq!(config, Config::default());
        assert!(config.record_revisions);
        assert!(!config.cross_vault_links);
    }

    #[test]
    fn configuration_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            default_vault: Some("Research".into()),
            cross_vault_links: true,
            models: ModelSettings {
                tagging: Some(ModelChoice {
                    provider: "ollama".into(),
                    model: "llama3.2".into(),
                    credential: None,
                    base_url: Some("http://localhost:11434".into()),
                    locality: None,
                    trusted_for_personal: false,
                }),
                ..ModelSettings::default()
            },
            ..Config::default()
        };

        config.save(dir.path()).unwrap();
        assert_eq!(Config::load(dir.path()).unwrap(), config);
    }

    #[test]
    fn a_saved_file_never_contains_a_credential() {
        // SEC-001 — asserted on the bytes, not just intended.
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            models: ModelSettings {
                summarization: Some(ModelChoice {
                    provider: "openrouter".into(),
                    model: "some-model".into(),
                    credential: Some("marius/openrouter".into()),
                    base_url: None,
                    locality: None,
                    trusted_for_personal: false,
                }),
                ..ModelSettings::default()
            },
            ..Config::default()
        };
        config.save(dir.path()).unwrap();

        let text = std::fs::read_to_string(dir.path().join(Config::FILE_NAME)).unwrap();
        assert!(
            text.contains("marius/openrouter"),
            "the reference is stored"
        );
        assert!(!text.contains("sk-"), "no key material");
        assert!(!text.to_lowercase().contains("api_key"));
        assert!(!text.to_lowercase().contains("secret"));
    }

    #[test]
    fn a_newer_config_is_refused_rather_than_misread() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(Config::FILE_NAME),
            r#"{"version": 99, "models": {}}"#,
        )
        .unwrap();
        assert!(matches!(
            Config::load(dir.path()),
            Err(ConfigError::TooNew { .. })
        ));
    }

    #[test]
    fn invalid_json_is_reported_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(Config::FILE_NAME), "{not json").unwrap();
        let err = Config::load(dir.path()).unwrap_err();
        assert!(err.to_string().contains("config.json"), "{err}");
    }

    #[test]
    fn a_saved_file_leaves_no_temporary_behind() {
        let dir = tempfile::tempdir().unwrap();
        Config::default().save(dir.path()).unwrap();
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![Config::FILE_NAME]);
    }

    #[test]
    fn the_data_directory_is_overridable_for_tests_and_portable_installs() {
        // Relied upon by the integration tests and by anyone running Marius
        // from a USB stick.
        let previous = std::env::var("MARIUS_DATA_DIR").ok();
        // SAFETY-adjacent note: this test mutates process-wide state, so it
        // restores it before returning.
        unsafe { std::env::set_var("MARIUS_DATA_DIR", "/tmp/marius-test") };
        assert_eq!(default_data_dir(), PathBuf::from("/tmp/marius-test"));
        match previous {
            Some(value) => unsafe { std::env::set_var("MARIUS_DATA_DIR", value) },
            None => unsafe { std::env::remove_var("MARIUS_DATA_DIR") },
        }
    }

    #[test]
    fn the_default_data_directory_follows_platform_convention() {
        let previous = std::env::var("MARIUS_DATA_DIR").ok();
        unsafe { std::env::remove_var("MARIUS_DATA_DIR") };
        let dir = default_data_dir();
        let text = dir.to_string_lossy();
        if cfg!(target_os = "macos") {
            assert!(
                text.contains("Library/Application Support/Marius"),
                "{text}"
            );
        } else if cfg!(target_os = "windows") {
            assert!(text.contains("Marius"), "{text}");
        } else {
            assert!(text.contains("marius"), "{text}");
        }
        if let Some(value) = previous {
            unsafe { std::env::set_var("MARIUS_DATA_DIR", value) };
        }
    }
}

//! Installed plugin packages and their capability grants.
//!
//! Requirements: PLUG-001, PLUG-006, PLUG-007, PLUG-008, SEC-009, AI-017.
//!
//! The model lives here, deliberately separate from any runtime, so that
//! storage and the settings UI can manage installations before the JavaScript
//! host exists (delivery phase 6).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::PluginInstallationId;

/// A capability a plugin must be granted explicitly.
///
/// PLUG-007 requires clear permissions, and SEC-009 forbids a plugin from
/// bypassing source protection or AI privacy rules. Capabilities are therefore
/// a closed set rather than free-form strings: the runtime matches on them, so
/// a new power cannot be invented by a manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Read documents in the vault through the safe abstraction (PLUG-005).
    ReadVault,
    /// Create and modify documents. Still mediated by the document service, so
    /// ARCH-012's protection of source content continues to apply.
    WriteVault,
    /// Read and write files outside any vault. Rarely justified.
    FileSystem,
    /// Make network requests.
    Network,
    /// Read stored credentials. Never granted implicitly (SEC-001).
    Secrets,
    /// Request inference. Routed through the policy engine like any other
    /// request, so a grant cannot widen a vault's privacy policy (SEC-009).
    Inference,
    /// Register commands in the palette.
    Commands,
    /// Contribute editor or view UI (PLUG-011).
    Ui,
}

impl Capability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadVault => "read_vault",
            Self::WriteVault => "write_vault",
            Self::FileSystem => "file_system",
            Self::Network => "network",
            Self::Secrets => "secrets",
            Self::Inference => "inference",
            Self::Commands => "commands",
            Self::Ui => "ui",
        }
    }

    /// True when granting this capability needs an explicit, per-install
    /// confirmation rather than being covered by a blanket "enable" action.
    pub const fn requires_explicit_consent(self) -> bool {
        matches!(
            self,
            Self::WriteVault | Self::FileSystem | Self::Network | Self::Secrets | Self::Inference
        )
    }

    pub const ALL: [Capability; 8] = [
        Self::ReadVault,
        Self::WriteVault,
        Self::FileSystem,
        Self::Network,
        Self::Secrets,
        Self::Inference,
        Self::Commands,
        Self::Ui,
    ];
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Capability {
    type Err = UnknownCapability;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "read_vault" => Ok(Self::ReadVault),
            "write_vault" => Ok(Self::WriteVault),
            "file_system" => Ok(Self::FileSystem),
            "network" => Ok(Self::Network),
            "secrets" => Ok(Self::Secrets),
            "inference" => Ok(Self::Inference),
            "commands" => Ok(Self::Commands),
            "ui" => Ok(Self::Ui),
            other => Err(UnknownCapability(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown capability {0:?}")]
pub struct UnknownCapability(pub String);

/// A plugin's declared identity, as read from its manifest.
///
/// Field names follow the Obsidian `manifest.json` convention so that an
/// unmodified plugin package can be read. Reading a documented file format is
/// not copying an implementation (PLUG-003, ARCH-014).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Minimum host version the plugin claims to need.
    #[serde(default, rename = "minAppVersion")]
    pub min_app_version: Option<String>,
    /// Obsidian's flag for "requires Electron/Node"; such plugins cannot run
    /// here and are reported as unsupported rather than half-loaded (PLUG-012).
    #[serde(default, rename = "isDesktopOnly")]
    pub desktop_only: bool,
}

/// Why a plugin cannot run, or runs only partly (PLUG-006).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityDiagnostic {
    /// The API or feature the plugin asked for.
    pub symbol: String,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// Implemented; listed for completeness in the compatibility report.
    Supported,
    /// Present but behaves differently, or is a no-op.
    Partial,
    /// Absent. A call will throw inside the plugin's own sandbox rather than
    /// taking the application down (PLUG-008).
    Unsupported,
}

impl DiagnosticSeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Partial => "partial",
            Self::Unsupported => "unsupported",
        }
    }
}

/// One installed plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInstallation {
    pub id: PluginInstallationId,
    pub manifest: PluginManifest,
    /// Directory the package was installed from (PLUG-001: manual install).
    pub install_path: std::path::PathBuf,
    pub enabled: bool,
    /// Capabilities the user granted. The runtime consults only this list;
    /// a manifest request is never a grant (PLUG-007).
    pub granted: Vec<Capability>,
    pub diagnostics: Vec<CompatibilityDiagnostic>,
    #[serde(with = "time::serde::rfc3339")]
    pub installed_at: OffsetDateTime,
}

impl PluginInstallation {
    pub fn new(manifest: PluginManifest, install_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            id: PluginInstallationId::new(),
            manifest,
            install_path: install_path.into(),
            // Installed disabled: a package that just landed on disk has not
            // been reviewed, and PLUG-001 separates install from enable.
            enabled: false,
            granted: Vec::new(),
            diagnostics: Vec::new(),
            installed_at: OffsetDateTime::now_utc(),
        }
    }

    /// True when the plugin holds `capability`.
    pub fn holds(&self, capability: Capability) -> bool {
        self.enabled && self.granted.contains(&capability)
    }

    /// Unsupported APIs this plugin touches (PLUG-006).
    pub fn unsupported_symbols(&self) -> Vec<&str> {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Unsupported)
            .map(|d| d.symbol.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PluginManifest {
        PluginManifest {
            id: "example-plugin".into(),
            name: "Example".into(),
            version: "1.2.3".into(),
            author: None,
            description: None,
            min_app_version: None,
            desktop_only: false,
        }
    }

    #[test]
    fn plugins_install_disabled_with_no_capabilities() {
        // PLUG-001 and PLUG-007: installing is not consenting.
        let install = PluginInstallation::new(manifest(), "/plugins/example");
        assert!(!install.enabled);
        assert!(install.granted.is_empty());
        assert!(!install.holds(Capability::ReadVault));
    }

    #[test]
    fn a_disabled_plugin_holds_nothing_even_when_granted() {
        let mut install = PluginInstallation::new(manifest(), "/plugins/example");
        install.granted.push(Capability::ReadVault);
        assert!(!install.holds(Capability::ReadVault));
        install.enabled = true;
        assert!(install.holds(Capability::ReadVault));
        assert!(!install.holds(Capability::Network));
    }

    #[test]
    fn dangerous_capabilities_need_explicit_consent() {
        // SEC-009.
        for cap in [
            Capability::WriteVault,
            Capability::FileSystem,
            Capability::Network,
            Capability::Secrets,
            Capability::Inference,
        ] {
            assert!(cap.requires_explicit_consent(), "{cap}");
        }
        for cap in [Capability::ReadVault, Capability::Commands, Capability::Ui] {
            assert!(!cap.requires_explicit_consent(), "{cap}");
        }
    }

    #[test]
    fn obsidian_manifests_parse_including_their_camel_case_fields() {
        // PLUG-003: reading a documented format, not reusing an implementation.
        let json = r#"{
            "id": "dataview",
            "name": "Dataview",
            "version": "0.5.0",
            "minAppVersion": "1.0.0",
            "author": "someone",
            "isDesktopOnly": true
        }"#;
        let parsed: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.id, "dataview");
        assert_eq!(parsed.min_app_version.as_deref(), Some("1.0.0"));
        assert!(parsed.desktop_only);
    }

    #[test]
    fn manifests_tolerate_missing_optional_fields() {
        let parsed: PluginManifest =
            serde_json::from_str(r#"{"id":"x","name":"X","version":"0.1"}"#).unwrap();
        assert!(!parsed.desktop_only);
        assert_eq!(parsed.author, None);
    }

    #[test]
    fn the_compatibility_report_names_unsupported_apis() {
        // PLUG-006.
        let mut install = PluginInstallation::new(manifest(), "/plugins/example");
        install.diagnostics = vec![
            CompatibilityDiagnostic {
                symbol: "Vault.read".into(),
                severity: DiagnosticSeverity::Supported,
                message: "implemented".into(),
            },
            CompatibilityDiagnostic {
                symbol: "requireApiVersion".into(),
                severity: DiagnosticSeverity::Partial,
                message: "always true".into(),
            },
            CompatibilityDiagnostic {
                symbol: "electron.remote".into(),
                severity: DiagnosticSeverity::Unsupported,
                message: "no Electron in Marius".into(),
            },
        ];
        assert_eq!(install.unsupported_symbols(), vec!["electron.remote"]);
    }

    #[test]
    fn capability_tokens_round_trip() {
        for cap in Capability::ALL {
            assert_eq!(cap, cap.as_str().parse().unwrap());
        }
    }
}

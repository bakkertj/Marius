//! Sensitivity levels and the data the privacy policy engine decides over.
//!
//! Requirements: ARCH-016, AI-005 .. AI-008, SEC-002, SEC-005, AC-016.
//!
//! The *types* live here so that storage, import and the UI can all speak about
//! sensitivity without depending on the inference stack; the decision procedure
//! lives in `marius-llm-policy`.

use serde::{Deserialize, Serialize};

use crate::ids::{DocumentId, VaultId};

/// How freely a document's content may leave the device.
///
/// Ordered from least to most restrictive, and the ordering is load-bearing:
/// [`Sensitivity::most_restrictive`] takes the maximum over every input, so a
/// single confidential source in a prompt restricts the whole request (AI-008).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    /// Any configured provider may see it.
    Unrestricted,
    /// Only providers the user has marked as trusted with personal material.
    /// This is the default for anything whose sensitivity is unknown, including
    /// every import — AI-007 requires defaulting conservatively, and silently
    /// treating unlabelled notes as public would be the wrong kind of wrong.
    #[default]
    Personal,
    /// Must not leave the device at all: local models only (AC-016).
    Confidential,
    /// No model may process it, local or remote. The escape hatch for material
    /// that should never enter a prompt.
    NoInference,
}

impl Sensitivity {
    /// The stricter of two levels.
    pub fn most_restrictive(self, other: Self) -> Self {
        self.max(other)
    }

    /// The strictest level across a set of documents about to be sent together.
    ///
    /// An empty set yields the conservative default rather than
    /// `Unrestricted` — "nothing to check" must never widen permission.
    pub fn across(levels: impl IntoIterator<Item = Self>) -> Self {
        levels
            .into_iter()
            .fold(Self::default(), Self::most_restrictive)
    }

    /// True when content at this level may be transmitted off the device.
    pub const fn allows_remote_inference(self) -> bool {
        matches!(self, Self::Unrestricted | Self::Personal)
    }

    /// True when content at this level may be processed by any model at all.
    pub const fn allows_any_inference(self) -> bool {
        !matches!(self, Self::NoInference)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unrestricted => "unrestricted",
            Self::Personal => "personal",
            Self::Confidential => "confidential",
            Self::NoInference => "no_inference",
        }
    }

    pub const ALL: [Sensitivity; 4] = [
        Self::Unrestricted,
        Self::Personal,
        Self::Confidential,
        Self::NoInference,
    ];
}

impl std::fmt::Display for Sensitivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Sensitivity {
    type Err = UnknownSensitivity;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "unrestricted" => Ok(Self::Unrestricted),
            "personal" => Ok(Self::Personal),
            "confidential" => Ok(Self::Confidential),
            "no_inference" => Ok(Self::NoInference),
            other => Err(UnknownSensitivity(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown sensitivity {0:?}")]
pub struct UnknownSensitivity(pub String);

/// Where a model runs, from the perspective of the data leaving the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Locality {
    /// Runs on this device; content never crosses a network boundary.
    Local,
    /// Reachable only on the user's own network or self-hosted host. Still a
    /// network hop, so it is *not* treated as local for `Confidential` content.
    SelfHosted,
    /// A third-party service.
    Remote,
}

impl Locality {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::SelfHosted => "self_hosted",
            Self::Remote => "remote",
        }
    }

    pub const ALL: [Locality; 3] = [Self::Local, Self::SelfHosted, Self::Remote];
}

impl std::fmt::Display for Locality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Locality {
    type Err = UnknownLocality;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "local" => Ok(Self::Local),
            "self_hosted" => Ok(Self::SelfHosted),
            "remote" => Ok(Self::Remote),
            other => Err(UnknownLocality(other.to_owned())),
        }
    }
}

/// The kind of work a model is being asked to do.
///
/// Requirements: AI-003 (independent model selection per task), AI-015 (track
/// task in the audit log).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceTask {
    /// Propose tags for a document (TAG-005).
    Tagging,
    /// Produce vectors for semantic search (SRCH-007).
    Embedding,
    /// Write a summary or synthesis note (AI-011).
    Summarization,
    /// Answer a grounded question over retrieved passages (SRCH-010).
    QuestionAnswering,
    /// Propose semantic relationships between documents (GRAPH-007).
    RelationshipDiscovery,
}

impl InferenceTask {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tagging => "tagging",
            Self::Embedding => "embedding",
            Self::Summarization => "summarization",
            Self::QuestionAnswering => "question_answering",
            Self::RelationshipDiscovery => "relationship_discovery",
        }
    }

    pub const ALL: [InferenceTask; 5] = [
        Self::Tagging,
        Self::Embedding,
        Self::Summarization,
        Self::QuestionAnswering,
        Self::RelationshipDiscovery,
    ];
}

impl std::fmt::Display for InferenceTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for InferenceTask {
    type Err = UnknownInferenceTask;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tagging" => Ok(Self::Tagging),
            "embedding" => Ok(Self::Embedding),
            "summarization" => Ok(Self::Summarization),
            "question_answering" => Ok(Self::QuestionAnswering),
            "relationship_discovery" => Ok(Self::RelationshipDiscovery),
            other => Err(UnknownInferenceTask(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown inference task {0:?}")]
pub struct UnknownInferenceTask(pub String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown locality {0:?}")]
pub struct UnknownLocality(pub String);

/// A vault's inference policy (SEC-005, AI-016).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultPolicy {
    pub vault: VaultId,
    /// Floor applied to every document in the vault. A document may be *more*
    /// restricted than its vault but never less — see [`VaultPolicy::effective`].
    pub default_sensitivity: Sensitivity,
    /// Tasks the user has switched off entirely for this vault.
    pub disabled_tasks: Vec<InferenceTask>,
    /// When set, only these providers may be used, whatever their locality.
    pub allowed_providers: Option<Vec<String>>,
    /// Providers that may never be used for this vault.
    pub denied_providers: Vec<String>,
    /// Whether a model may propose tags without the user reviewing them first
    /// (AI-010). Independent of transmission policy.
    pub auto_classify: bool,
}

impl VaultPolicy {
    /// A policy that permits local inference of personal material and nothing
    /// looser — the default a newly created vault gets.
    pub fn conservative(vault: VaultId) -> Self {
        Self {
            vault,
            default_sensitivity: Sensitivity::default(),
            disabled_tasks: Vec::new(),
            allowed_providers: None,
            denied_providers: Vec::new(),
            auto_classify: false,
        }
    }

    /// Combine the vault floor with a document's own override.
    ///
    /// AI-007 forbids automatically *lowering* a restriction, so a document
    /// override can only tighten: the result is the stricter of the two.
    pub fn effective(&self, document_override: Option<Sensitivity>) -> Sensitivity {
        match document_override {
            Some(level) => self.default_sensitivity.most_restrictive(level),
            None => self.default_sensitivity,
        }
    }

    /// True when the user has switched this task off for the whole vault.
    pub fn task_disabled(&self, task: InferenceTask) -> bool {
        self.disabled_tasks.contains(&task)
    }

    /// True when `provider` is permitted by the allow/deny lists.
    pub fn permits_provider(&self, provider: &str) -> bool {
        if self.denied_providers.iter().any(|p| p == provider) {
            return false;
        }
        match &self.allowed_providers {
            Some(allowed) => allowed.iter().any(|p| p == provider),
            None => true,
        }
    }
}

/// A per-document restriction (SEC-005).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentPolicy {
    pub document: DocumentId,
    pub sensitivity: Sensitivity,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_sensitivity_defaults_conservatively() {
        // AI-007: an unlabelled or imported document is not public.
        assert_eq!(Sensitivity::default(), Sensitivity::Personal);
        // Personal material may still reach a provider the user trusts; it is
        // `Confidential` that pins content to the device.
        assert!(Sensitivity::Personal.allows_remote_inference());
        assert!(!Sensitivity::Confidential.allows_remote_inference());
    }

    #[test]
    fn strictest_level_wins_across_a_prompt() {
        // AI-008: one confidential source restricts the entire request.
        let level = Sensitivity::across([
            Sensitivity::Unrestricted,
            Sensitivity::Confidential,
            Sensitivity::Unrestricted,
        ]);
        assert_eq!(level, Sensitivity::Confidential);
        assert!(!level.allows_remote_inference());
    }

    #[test]
    fn empty_source_set_does_not_widen_permission() {
        assert_eq!(
            Sensitivity::across(std::iter::empty()),
            Sensitivity::Personal
        );
    }

    #[test]
    fn no_inference_blocks_even_local_models() {
        assert!(!Sensitivity::NoInference.allows_any_inference());
        assert!(Sensitivity::Confidential.allows_any_inference());
    }

    #[test]
    fn document_override_can_only_tighten() {
        let mut policy = VaultPolicy::conservative(VaultId::new());
        policy.default_sensitivity = Sensitivity::Confidential;
        // Trying to relax a confidential vault to unrestricted has no effect.
        assert_eq!(
            policy.effective(Some(Sensitivity::Unrestricted)),
            Sensitivity::Confidential
        );
        // Tightening further is honoured.
        assert_eq!(
            policy.effective(Some(Sensitivity::NoInference)),
            Sensitivity::NoInference
        );
    }

    #[test]
    fn provider_allow_list_is_exclusive_and_deny_list_wins() {
        let mut policy = VaultPolicy::conservative(VaultId::new());
        assert!(policy.permits_provider("openrouter"));

        policy.allowed_providers = Some(vec!["ollama".into()]);
        assert!(policy.permits_provider("ollama"));
        assert!(!policy.permits_provider("openrouter"));

        policy.denied_providers.push("ollama".into());
        assert!(!policy.permits_provider("ollama"));
    }

    #[test]
    fn tokens_round_trip() {
        for s in Sensitivity::ALL {
            assert_eq!(s, s.as_str().parse().unwrap());
        }
        for l in Locality::ALL {
            assert_eq!(l, l.as_str().parse().unwrap());
        }
        for t in InferenceTask::ALL {
            assert_eq!(t, t.as_str().parse().unwrap());
        }
    }
}

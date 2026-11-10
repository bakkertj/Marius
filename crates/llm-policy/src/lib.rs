//! The privacy policy engine (ARCH-016, AI-005 .. AI-008, SEC-005, AC-016).
//!
//! One pure function, [`decide`], stands between every inference feature and
//! the network. It never performs I/O: callers gather the inputs (the vault's
//! policy, the effective sensitivity of the material, the configured
//! providers), and it returns either the provider that may run the request or
//! the reason nothing may — in words fit for the audit log, because every
//! decision becomes an [`marius_model::InferenceRecord`] whether it was
//! allowed or not.
//!
//! The ordering of checks is deliberate and load-bearing: the *material's*
//! restrictions are examined before any provider is, so a `NoInference`
//! document produces "denied by policy" even on a machine with no providers
//! configured at all. "There was nothing eligible" and "this must not run"
//! are different answers to a privacy question (AI-004 vs AC-016).

use marius_model::{InferenceTask, Locality, Sensitivity, VaultPolicy};
use serde::{Deserialize, Serialize};

/// A provider as the policy engine sees it: no client, no credentials, just
/// the facts a decision needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// Identifier the allow/deny lists match against, e.g. `ollama`.
    pub name: String,
    pub model: String,
    pub locality: Locality,
    /// The user has marked this provider as acceptable for `Personal`
    /// material (AI-006). Meaningless for `Local` providers, which may
    /// always see it — nothing leaves the device.
    pub trusted_for_personal: bool,
}

impl ProviderProfile {
    /// A provider running on this machine (an Ollama daemon, typically).
    pub fn local(name: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            model: model.into(),
            locality: Locality::Local,
            trusted_for_personal: true,
        }
    }
}

/// The verdict, with the engine's reasoning in audit-log words.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Decision {
    /// `provider` may run the request.
    Approved {
        provider: ProviderProfile,
        reason: String,
    },
    /// The request must not run at all, whatever is configured (AC-016).
    Denied { reason: String },
    /// The request could run in principle, but nothing configured is
    /// eligible. Core features stay usable (AI-004).
    NoEligibleProvider { reason: String },
}

impl Decision {
    pub fn approved(&self) -> Option<&ProviderProfile> {
        match self {
            Self::Approved { provider, .. } => Some(provider),
            _ => None,
        }
    }

    pub fn reason(&self) -> &str {
        match self {
            Self::Approved { reason, .. }
            | Self::Denied { reason }
            | Self::NoEligibleProvider { reason } => reason,
        }
    }
}

/// May material at `sensitivity` be shown to a provider at `locality`?
///
/// The table the whole engine rests on (AI-005, AI-006, AC-016):
///
/// | sensitivity    | local | self-hosted      | remote           |
/// |----------------|-------|------------------|------------------|
/// | unrestricted   | yes   | yes              | yes              |
/// | personal       | yes   | if trusted       | if trusted       |
/// | confidential   | yes   | no               | no               |
/// | no_inference   | no    | no               | no               |
///
/// Self-hosted is a network hop, so it is *not* local for confidential
/// material — "my own server" is still an exfiltration path for a document
/// the user said must not leave the device.
pub fn locality_permitted(
    sensitivity: Sensitivity,
    locality: Locality,
    trusted_for_personal: bool,
) -> bool {
    match sensitivity {
        Sensitivity::NoInference => false,
        Sensitivity::Confidential => locality == Locality::Local,
        Sensitivity::Personal => locality == Locality::Local || trusted_for_personal,
        Sensitivity::Unrestricted => true,
    }
}

/// Decide whether — and with which provider — one request may run.
///
/// `sensitivity` must already be the *effective* level: the vault floor
/// combined with every source document's override, folded with
/// [`Sensitivity::across`] so one confidential source restricts the whole
/// request (AI-008). `providers` is in preference order; the first eligible
/// one wins.
pub fn decide(
    policy: &VaultPolicy,
    task: InferenceTask,
    sensitivity: Sensitivity,
    providers: &[ProviderProfile],
) -> Decision {
    // The material's own restrictions come first: these produce denials
    // regardless of configuration.
    if !sensitivity.allows_any_inference() {
        return Decision::Denied {
            reason: format!("content is marked {sensitivity}: no model may process it"),
        };
    }
    if policy.task_disabled(task) {
        return Decision::Denied {
            reason: format!("{task} is switched off for this vault"),
        };
    }

    // Now the configuration: first eligible provider in preference order.
    let mut rejections: Vec<String> = Vec::new();
    for provider in providers {
        if !policy.permits_provider(&provider.name) {
            rejections.push(format!("{}: excluded by vault policy", provider.name));
            continue;
        }
        if !locality_permitted(
            sensitivity,
            provider.locality,
            provider.trusted_for_personal,
        ) {
            rejections.push(format!(
                "{}: {} provider may not see {} content",
                provider.name, provider.locality, sensitivity
            ));
            continue;
        }
        return Decision::Approved {
            provider: provider.clone(),
            reason: format!(
                "{} ({}, {}) permitted for {} content",
                provider.name, provider.model, provider.locality, sensitivity
            ),
        };
    }

    if providers.is_empty() {
        return Decision::NoEligibleProvider {
            reason: format!("no provider is configured for {task}"),
        };
    }
    Decision::NoEligibleProvider {
        reason: rejections.join("; "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marius_model::VaultId;

    fn providers() -> Vec<ProviderProfile> {
        vec![
            ProviderProfile::local("ollama", "llama3.2"),
            ProviderProfile {
                name: "openrouter".into(),
                model: "big-model".into(),
                locality: Locality::Remote,
                trusted_for_personal: false,
            },
        ]
    }

    fn policy() -> VaultPolicy {
        VaultPolicy::conservative(VaultId::new())
    }

    #[test]
    fn confidential_content_never_leaves_the_device() {
        // AC-016. Remote-only configuration + confidential content = nothing
        // eligible; with a local provider present, the local one is chosen.
        let remote_only = &providers()[1..];
        let decision = decide(
            &policy(),
            InferenceTask::Summarization,
            Sensitivity::Confidential,
            remote_only,
        );
        assert!(matches!(decision, Decision::NoEligibleProvider { .. }));
        assert!(decision.reason().contains("may not see confidential"));

        let decision = decide(
            &policy(),
            InferenceTask::Summarization,
            Sensitivity::Confidential,
            &providers(),
        );
        assert_eq!(decision.approved().unwrap().name, "ollama");
    }

    #[test]
    fn no_inference_is_denied_even_with_no_providers_configured() {
        // "Must not run" and "nothing to run on" are different answers.
        let decision = decide(
            &policy(),
            InferenceTask::Tagging,
            Sensitivity::NoInference,
            &[],
        );
        assert!(matches!(decision, Decision::Denied { .. }));

        let decision = decide(
            &policy(),
            InferenceTask::Tagging,
            Sensitivity::Personal,
            &[],
        );
        assert!(matches!(decision, Decision::NoEligibleProvider { .. }));
    }

    #[test]
    fn personal_content_needs_trust_to_go_remote() {
        // AI-006: the default remote provider is not trusted with personal
        // material until the user says so.
        let remote_only = &providers()[1..];
        let decision = decide(
            &policy(),
            InferenceTask::Tagging,
            Sensitivity::Personal,
            remote_only,
        );
        assert!(matches!(decision, Decision::NoEligibleProvider { .. }));

        let mut trusted = remote_only.to_vec();
        trusted[0].trusted_for_personal = true;
        let decision = decide(
            &policy(),
            InferenceTask::Tagging,
            Sensitivity::Personal,
            &trusted,
        );
        assert_eq!(decision.approved().unwrap().name, "openrouter");
    }

    #[test]
    fn self_hosted_is_not_local_for_confidential_material() {
        let self_hosted = vec![ProviderProfile {
            name: "my-server".into(),
            model: "llama3.2".into(),
            locality: Locality::SelfHosted,
            trusted_for_personal: true,
        }];
        let decision = decide(
            &policy(),
            InferenceTask::QuestionAnswering,
            Sensitivity::Confidential,
            &self_hosted,
        );
        assert!(matches!(decision, Decision::NoEligibleProvider { .. }));

        // Personal material may reach it once trusted.
        let decision = decide(
            &policy(),
            InferenceTask::QuestionAnswering,
            Sensitivity::Personal,
            &self_hosted,
        );
        assert!(decision.approved().is_some());
    }

    #[test]
    fn disabled_tasks_and_denied_providers_are_honoured() {
        let mut p = policy();
        p.disabled_tasks.push(InferenceTask::Embedding);
        let decision = decide(
            &p,
            InferenceTask::Embedding,
            Sensitivity::Unrestricted,
            &providers(),
        );
        assert!(matches!(decision, Decision::Denied { .. }));

        let mut p = policy();
        p.denied_providers.push("ollama".into());
        let decision = decide(
            &p,
            InferenceTask::Tagging,
            Sensitivity::Confidential,
            &providers(),
        );
        // The only provider allowed to see confidential content is denied by
        // name; the remote one is ineligible by locality.
        assert!(matches!(decision, Decision::NoEligibleProvider { .. }));
        assert!(decision.reason().contains("excluded by vault policy"));
    }

    #[test]
    fn preference_order_wins_among_eligible_providers() {
        let mut both = providers();
        both[1].trusted_for_personal = true;
        // Unrestricted content: both eligible; the first listed is chosen.
        let decision = decide(
            &policy(),
            InferenceTask::Tagging,
            Sensitivity::Unrestricted,
            &both,
        );
        assert_eq!(decision.approved().unwrap().name, "ollama");

        both.swap(0, 1);
        let decision = decide(
            &policy(),
            InferenceTask::Tagging,
            Sensitivity::Unrestricted,
            &both,
        );
        assert_eq!(decision.approved().unwrap().name, "openrouter");
    }

    #[test]
    fn the_locality_table_is_exactly_as_documented() {
        use Locality::*;
        use Sensitivity::*;
        let table = [
            (Unrestricted, Local, false, true),
            (Unrestricted, SelfHosted, false, true),
            (Unrestricted, Remote, false, true),
            (Personal, Local, false, true),
            (Personal, SelfHosted, false, false),
            (Personal, SelfHosted, true, true),
            (Personal, Remote, true, true),
            (Confidential, Local, true, true),
            (Confidential, SelfHosted, true, false),
            (Confidential, Remote, true, false),
            (NoInference, Local, true, false),
            (NoInference, Remote, true, false),
        ];
        for (sensitivity, locality, trusted, expected) in table {
            assert_eq!(
                locality_permitted(sensitivity, locality, trusted),
                expected,
                "{sensitivity} / {locality} / trusted={trusted}"
            );
        }
    }
}

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Component, PathBuf};

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct RouteKey(String);
impl fmt::Debug for RouteKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RouteKey(<redacted>)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AccountCacheKey {
    route: RouteKey,
    resolved_account_id: String,
}
impl AccountCacheKey {
    pub(crate) fn route(&self) -> &RouteKey {
        &self.route
    }
    pub(crate) fn resolved_account_id(&self) -> &str {
        &self.resolved_account_id
    }
}

/// P2's internal descriptor. It is deliberately not part of a Tauri command.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodexProfileInput {
    pub(crate) profile_id: String,
    pub(crate) home: Option<PathBuf>,
}
impl fmt::Debug for CodexProfileInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexProfileInput")
            .field("profile_id", &"<redacted>")
            .field("home", &"<redacted>")
            .finish()
    }
}

/// Credential route is not caller label: custom routes use a canonical absolute home identity.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct CodexProfile {
    profile_id: String,
    home: Option<PathBuf>,
    route: RouteKey,
}
impl fmt::Debug for CodexProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexProfile")
            .field("profile_id", &self.profile_id)
            .field("home", &"<redacted>")
            .finish()
    }
}
impl CodexProfile {
    pub(crate) fn from_registry(alias: String, canonical_home: PathBuf) -> Self {
        Self {
            profile_id: format!("codex/{alias}"),
            route: RouteKey(format!("codex-home:{}", canonical_home.display())),
            home: Some(canonical_home),
        }
    }
    pub(crate) fn from_input(input: CodexProfileInput) -> Result<Self, String> {
        if !input.profile_id.starts_with("codex/")
            || input.profile_id.len() == "codex/".len()
            || input.profile_id.chars().any(char::is_control)
        {
            return Err("Codex profile_id must be a non-empty codex/<name> identifier".to_string());
        }
        match input.home {
            None => {
                if input.profile_id != "codex/default" {
                    return Err(
                        "The default Codex credential route must use profile_id codex/default"
                            .to_string(),
                    );
                }
                Ok(Self::default())
            }
            Some(home) => {
                if input.profile_id == "codex/default" {
                    return Err("codex/default cannot specify a custom home".to_string());
                }
                if !home.is_absolute()
                    || home
                        .components()
                        .any(|component| matches!(component, Component::ParentDir))
                {
                    return Err(
                        "Codex profile home must be an absolute path without .. segments"
                            .to_string(),
                    );
                }
                let canonical = home
                    .canonicalize()
                    .map_err(|_| "Could not resolve Codex profile home".to_string())?;
                Ok(Self {
                    profile_id: input.profile_id,
                    route: RouteKey(format!("codex-home:{}", canonical.display())),
                    home: Some(canonical),
                })
            }
        }
    }
    pub(crate) fn default() -> Self {
        Self {
            profile_id: "codex/default".to_string(),
            home: None,
            route: RouteKey("codex-default".to_string()),
        }
    }
    pub(crate) fn profile_id(&self) -> &str {
        &self.profile_id
    }
    pub(crate) fn home(&self) -> Option<&PathBuf> {
        self.home.as_ref()
    }
    pub(crate) fn route(&self) -> &RouteKey {
        &self.route
    }
    pub(crate) fn cache_key(&self, resolved_account_id: impl Into<String>) -> AccountCacheKey {
        AccountCacheKey {
            route: self.route.clone(),
            resolved_account_id: resolved_account_id.into(),
        }
    }
}

/// The only custom-profile data serialized across the webview boundary.
/// Private P2 route, account, and credential data must stay backend-only.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodexProfilePublicQuota {
    pub(crate) alias: String,
    pub(crate) status: String,
    pub(crate) plan_type: Option<String>,
    pub(crate) primary: Option<crate::domain::models::CodexRateLimitWindow>,
    pub(crate) secondary: Option<crate::domain::models::CodexRateLimitWindow>,
    pub(crate) available_reset_credits: u32,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodexProfilesResponse {
    pub(crate) profiles: Vec<CodexProfilePublicQuota>,
    pub(crate) registry_error: Option<String>,
}

impl CodexProfilePublicQuota {
    pub(crate) fn unavailable(alias: String) -> Self {
        Self {
            alias,
            status: "error".to_string(),
            plan_type: None,
            primary: None,
            secondary: None,
            available_reset_credits: 0,
            error: Some("Profile configuration is invalid".to_string()),
        }
    }
}

/// P2's private aggregation shape. It must never be returned from IPC.
#[derive(Clone)]
pub(crate) struct CodexProfileQuota {
    pub(crate) profile_id: String,
    pub(crate) account_id: Option<String>,
    pub(crate) info: crate::domain::models::CodexData,
    pub(crate) rate_limits: crate::domain::models::CodexRateLimits,
    pub(crate) reset_credits: crate::domain::models::CodexResetCredits,
}
impl fmt::Debug for CodexProfileQuota {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexProfileQuota")
            .field("profile_id", &"<redacted>")
            .field("account_id", &"<redacted>")
            .field("info", &self.info)
            .field("rate_limits", &self.rate_limits)
            .field("reset_credits", &self.reset_credits)
            .finish()
    }
}
impl CodexProfileQuota {
    pub(crate) fn disconnected(profile_id: String, error: impl Into<String>) -> Self {
        let error = error.into();
        Self {
            profile_id,
            account_id: None,
            info: crate::domain::models::CodexData::disconnected(error.clone()),
            rate_limits: crate::domain::models::CodexRateLimits::disconnected(error.clone()),
            reset_credits: crate::domain::models::CodexResetCredits::disconnected(error),
        }
    }
}

pub(crate) fn default_codex_profile() -> CodexProfile {
    CodexProfile::default()
}
pub(crate) fn default_codex_cache_key(id: impl Into<String>) -> AccountCacheKey {
    default_codex_profile().cache_key(id)
}

#[cfg(test)]
mod tests {
    use super::{
        default_codex_cache_key, default_codex_profile, CodexProfile, CodexProfileInput,
        CodexProfilePublicQuota,
    };
    use std::fs;
    use std::path::PathBuf;
    #[test]
    fn default_descriptor_is_codex_default() {
        assert_eq!(default_codex_profile().profile_id(), "codex/default");
        assert_eq!(
            default_codex_cache_key("acct-a").resolved_account_id(),
            "acct-a"
        );
    }
    #[test]
    fn explicit_home_cannot_collide_with_default_or_another_home() {
        let base = std::env::temp_dir().join(format!("quotabar-route-{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        assert!(CodexProfile::from_input(CodexProfileInput {
            profile_id: "codex/default".into(),
            home: Some(a.clone())
        })
        .is_err());
        let first = CodexProfile::from_input(CodexProfileInput {
            profile_id: "codex/work".into(),
            home: Some(a),
        })
        .unwrap();
        let second = CodexProfile::from_input(CodexProfileInput {
            profile_id: "codex/work".into(),
            home: Some(b),
        })
        .unwrap();
        assert_ne!(first.route(), second.route());
        assert_ne!(first.cache_key("acct"), second.cache_key("acct"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn public_profile_dto_redacts_private_identity_material() {
        let dto = CodexProfilePublicQuota {
            alias: "work".into(),
            status: "connected".into(),
            plan_type: Some("pro".into()),
            primary: None,
            secondary: None,
            available_reset_credits: 1,
            error: None,
        };
        let output = serde_json::to_value(&dto).unwrap();
        let keys = output
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let expected_keys = [
            "alias",
            "availableResetCredits",
            "error",
            "planType",
            "primary",
            "secondary",
            "status",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(keys, expected_keys);
        assert!(!format!("{dto:?}").contains("/private/credential-route"));
    }

    #[test]
    fn private_debug_output_redacts_populated_identity_and_path_values() {
        use crate::domain::models::{CodexData, CodexRateLimits, CodexResetCredits};

        let input = CodexProfileInput {
            profile_id: "codex/secret-profile".into(),
            home: Some(PathBuf::from("/private/credential-route")),
        };
        let quota = super::CodexProfileQuota {
            profile_id: "codex/secret-profile".into(),
            account_id: Some("acct-secret".into()),
            info: CodexData {
                connected: true,
                plan_type: Some("pro".into()),
                account_id: Some("acct-secret".into()),
                subscription_until: None,
                email: Some("private@example.test".into()),
                error: Some("Bearer token-like-secret".into()),
            },
            rate_limits: CodexRateLimits::disconnected("offline"),
            reset_credits: CodexResetCredits::disconnected("offline"),
        };
        let debug = format!("{input:?} {quota:?}");
        for secret in [
            "/private/credential-route",
            "secret-profile",
            "acct-secret",
            "private@example.test",
            "token-like-secret",
        ] {
            assert!(!debug.contains(secret), "debug exposed {secret}");
        }
    }
}

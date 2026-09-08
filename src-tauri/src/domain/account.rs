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

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexProfileInput {
    pub profile_id: String,
    pub home: Option<PathBuf>,
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexProfileQuota {
    pub profile_id: String,
    pub account_id: Option<String>,
    pub info: crate::domain::models::CodexData,
    pub rate_limits: crate::domain::models::CodexRateLimits,
    pub reset_credits: crate::domain::models::CodexResetCredits,
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
    use super::{default_codex_cache_key, default_codex_profile, CodexProfile, CodexProfileInput};
    use std::fs;
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
}

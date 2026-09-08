use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProviderId(&'static str);

impl ProviderId {
    const CODEX: Self = Self("codex");
    #[cfg(test)]
    const fn synthetic(value: &'static str) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AccountId {
    provider: ProviderId,
    opaque_id: String,
}
impl AccountId {
    fn new(provider: ProviderId, opaque_id: impl Into<String>) -> Self {
        Self {
            provider,
            opaque_id: opaque_id.into(),
        }
    }
}

/// Non-secret, caller-supplied credential route. The home is never logged or returned.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexProfileInput {
    pub profile_id: String,
    pub home: Option<PathBuf>,
}

/// Canonical credential route; profile identity is deliberately separate from JWT claims.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CodexProfile {
    profile_id: String,
    home: Option<PathBuf>,
}
impl CodexProfile {
    pub(crate) fn from_input(input: CodexProfileInput) -> Result<Self, String> {
        if !input.profile_id.starts_with("codex/")
            || input.profile_id.len() == "codex/".len()
            || input.profile_id.chars().any(char::is_control)
        {
            return Err("Codex profile_id must be a non-empty codex/<name> identifier".to_string());
        }
        Ok(Self {
            profile_id: input.profile_id,
            home: input.home,
        })
    }
    pub(crate) fn default() -> Self {
        Self {
            profile_id: "codex/default".to_string(),
            home: None,
        }
    }
    pub(crate) fn profile_id(&self) -> &str {
        &self.profile_id
    }
    pub(crate) fn home(&self) -> Option<&PathBuf> {
        self.home.as_ref()
    }
    fn route_account_id(&self) -> AccountId {
        AccountId::new(ProviderId::CODEX, self.profile_id.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountDescriptor {
    identity: AccountId,
    profile: CodexProfile,
}
impl AccountDescriptor {
    pub(crate) fn cache_key_for_resolved_id(
        &self,
        opaque_id: impl Into<String>,
    ) -> AccountCacheKey {
        AccountCacheKey {
            route: self.identity.clone(),
            resolved: AccountId::new(ProviderId::CODEX, opaque_id),
        }
    }
    pub(crate) fn profile(&self) -> &CodexProfile {
        &self.profile
    }
}

/// Cache keys include both credential route and opaque claim. Duplicate claims remain isolated.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AccountCacheKey {
    route: AccountId,
    resolved: AccountId,
}
impl AccountCacheKey {
    pub(crate) fn profile_id(&self) -> &str {
        &self.route.opaque_id
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

pub(crate) fn codex_account(profile: CodexProfile) -> AccountDescriptor {
    AccountDescriptor {
        identity: profile.route_account_id(),
        profile,
    }
}
pub(crate) fn default_codex_account() -> AccountDescriptor {
    codex_account(CodexProfile::default())
}
pub(crate) fn default_codex_profile() -> CodexProfile {
    CodexProfile::default()
}
pub(crate) fn default_codex_cache_key(resolved_account_id: impl Into<String>) -> AccountCacheKey {
    default_codex_account().cache_key_for_resolved_id(resolved_account_id)
}

#[cfg(test)]
mod tests {
    use super::{
        codex_account, default_codex_account, default_codex_cache_key, AccountCacheKey,
        CodexProfile, CodexProfileInput, ProviderId,
    };
    use std::collections::HashSet;
    use std::path::PathBuf;

    #[test]
    fn default_descriptor_is_stable_and_non_secret() {
        let descriptor = default_codex_account();
        assert_eq!(descriptor.profile().profile_id(), "codex/default");
        let key = default_codex_cache_key("acct-a");
        assert_eq!(key.resolved.provider, ProviderId::CODEX);
        assert_eq!(key.resolved.opaque_id, "acct-a");
    }

    #[test]
    fn route_identity_remains_distinct_from_resolved_claim() {
        let profile = CodexProfile::from_input(CodexProfileInput {
            profile_id: "codex/work".into(),
            home: Some(PathBuf::from("/tmp/work")),
        })
        .unwrap();
        let key = codex_account(profile).cache_key_for_resolved_id("acct-a");
        assert_ne!(key.route.opaque_id, key.resolved.opaque_id);
    }

    #[test]
    fn duplicate_resolved_accounts_keep_separate_route_keys() {
        let a = CodexProfile::from_input(CodexProfileInput {
            profile_id: "codex/a".into(),
            home: None,
        })
        .unwrap();
        let b = CodexProfile::from_input(CodexProfileInput {
            profile_id: "codex/b".into(),
            home: None,
        })
        .unwrap();
        let keys = HashSet::<AccountCacheKey>::from([
            codex_account(a).cache_key_for_resolved_id("acct"),
            codex_account(b).cache_key_for_resolved_id("acct"),
        ]);
        assert_eq!(keys.len(), 2);
    }
}

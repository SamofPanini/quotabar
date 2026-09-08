/// Stable provider namespace used as part of every account identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProviderId(&'static str);

impl ProviderId {
    const CODEX: Self = Self("codex");

    #[cfg(test)]
    const fn synthetic(value: &'static str) -> Self {
        Self(value)
    }
}

/// Opaque, provider-namespaced account identity.
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

/// Non-secret description of the existing credential resolution route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CredentialSource {
    DefaultCodexHome,
}

/// Minimal metadata for an account available to a provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountDescriptor {
    identity: AccountId,
    credential_source: CredentialSource,
}

impl AccountDescriptor {
    pub(crate) fn cache_key_for_resolved_id(
        &self,
        opaque_id: impl Into<String>,
    ) -> AccountCacheKey {
        AccountCacheKey(AccountId::new(self.identity.provider.clone(), opaque_id))
    }
}

/// Typed key that prevents account IDs from colliding across providers.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AccountCacheKey(AccountId);

/// Internal association between quota data and the account it describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QuotaSnapshot<T> {
    pub(crate) account: AccountId,
    pub(crate) quota: T,
}

/// Adapts the unchanged current Codex credential route into the account domain.
pub(crate) fn default_codex_account() -> AccountDescriptor {
    AccountDescriptor {
        identity: AccountId::new(ProviderId::CODEX, "default"),
        credential_source: CredentialSource::DefaultCodexHome,
    }
}

pub(crate) fn default_codex_cache_key(resolved_account_id: impl Into<String>) -> AccountCacheKey {
    default_codex_account().cache_key_for_resolved_id(resolved_account_id)
}

#[cfg(test)]
mod tests {
    use super::{
        default_codex_account, AccountCacheKey, AccountId, CredentialSource, ProviderId,
        QuotaSnapshot,
    };
    use std::collections::HashSet;

    #[test]
    fn default_account_is_deterministic_and_non_secret() {
        let first = default_codex_account();
        let second = default_codex_account();

        assert_eq!(first, second);
        assert_eq!(first.credential_source, CredentialSource::DefaultCodexHome);

        let metadata = format!("{:?}", first.credential_source).to_lowercase();
        for secret_field in [
            "access_token",
            "refresh_token",
            "authorization",
            "cookie",
            "session",
            "auth.json",
        ] {
            assert!(!metadata.contains(secret_field));
        }
    }

    #[test]
    fn cache_keys_are_equal_only_for_the_same_provider_and_account() {
        let provider = ProviderId::synthetic("provider-a");
        let same_a = AccountCacheKey(AccountId::new(provider.clone(), "account-a"));
        let same_b = AccountCacheKey(AccountId::new(provider.clone(), "account-a"));
        let different_account = AccountCacheKey(AccountId::new(provider, "account-b"));
        let different_provider = AccountCacheKey(AccountId::new(
            ProviderId::synthetic("provider-b"),
            "account-a",
        ));

        assert_eq!(same_a, same_b);
        assert_ne!(same_a, different_account);
        assert_ne!(same_a, different_provider);

        let keys = HashSet::from([same_a, same_b, different_account, different_provider]);
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn quota_snapshot_keeps_its_account_association() {
        let account = AccountId::new(ProviderId::synthetic("provider-a"), "account-a");
        let snapshot = QuotaSnapshot {
            account: account.clone(),
            quota: 42_u8,
        };

        assert_eq!(snapshot.account, account);
        assert_eq!(snapshot.quota, 42);
    }
}

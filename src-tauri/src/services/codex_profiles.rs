use crate::domain::account::{CodexProfile, CodexProfilePublicQuota, CodexProfilesResponse};
use crate::services::codex;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub(crate) const CONFIG_FILE: &str = "codex-profiles.json";
/// Defensive cap: custom profiles are fetched sequentially on a refresh.
const MAX_CUSTOM_PROFILES: usize = 12;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u32,
    profiles: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigProfile {
    alias: String,
    home: PathBuf,
}

pub(crate) struct Registry {
    entries: Vec<RegistryEntry>,
    error: Option<String>,
}

enum RegistryEntry {
    Valid {
        alias: String,
        profile: CodexProfile,
    },
    Invalid {
        alias: String,
    },
}

fn safe_alias(value: &str) -> Option<String> {
    let value = value.trim();
    ((1..=48).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '/' && c != '\\')
        && !value.eq_ignore_ascii_case("default"))
    .then(|| value.to_string())
}

fn invalid_alias(index: usize, reserved: &HashSet<String>, used: &mut HashSet<String>) -> String {
    let base = format!("profile-{}", index + 1);
    let mut suffix = 0;
    loop {
        let candidate = if suffix == 0 {
            base.clone()
        } else {
            format!("{base}-invalid-{suffix}")
        };
        if !reserved.contains(&candidate) && used.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

pub(crate) fn load_registry(config_dir: &Path, default_home: Option<&Path>) -> Registry {
    let content = match fs::read_to_string(config_dir.join(CONFIG_FILE)) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Registry {
                entries: vec![],
                error: None,
            };
        }
        Err(_) => {
            return Registry {
                entries: vec![],
                error: Some("Profile configuration is unavailable".into()),
            };
        }
    };
    let config: Config = match serde_json::from_str::<Config>(&content) {
        Ok(config) if config.version == 1 => config,
        _ => {
            return Registry {
                entries: vec![],
                error: Some("Profile configuration is invalid".into()),
            };
        }
    };

    let overflow = config.profiles.len() > MAX_CUSTOM_PROFILES;
    // Reserve every syntactically safe alias first so an invalid row's generated
    // label can never shadow a valid alias declared later in the file.
    let reserved_aliases = config
        .profiles
        .iter()
        .filter_map(|entry| entry.get("alias")?.as_str())
        .filter_map(safe_alias)
        .collect::<HashSet<_>>();
    let default_home = default_home.and_then(|home| home.canonicalize().ok());
    let mut aliases = HashSet::new();
    let mut routes = HashSet::new();
    let mut entries = Vec::new();
    for (index, raw_entry) in config.profiles.into_iter().enumerate() {
        if index == MAX_CUSTOM_PROFILES {
            break;
        }
        let entry = serde_json::from_value::<ConfigProfile>(raw_entry);
        let valid_alias = entry
            .as_ref()
            .ok()
            .and_then(|entry| safe_alias(&entry.alias))
            .filter(|alias| aliases.insert(alias.clone()));
        let alias = valid_alias
            .clone()
            .unwrap_or_else(|| invalid_alias(index, &reserved_aliases, &mut aliases));
        let home = entry.ok().and_then(|entry| {
            (entry.home.is_absolute()
                && !entry
                    .home
                    .components()
                    .any(|part| matches!(part, Component::ParentDir)))
            .then(|| entry.home.canonicalize().ok())
            .flatten()
            .filter(|home| home.is_dir())
        });
        match (valid_alias, home) {
            (Some(alias), Some(home))
                if default_home
                    .as_ref()
                    .is_some_and(|default| default == &home) =>
            {
                entries.push(RegistryEntry::Invalid { alias });
            }
            (Some(alias), Some(home)) if routes.insert(home.clone()) => {
                entries.push(RegistryEntry::Valid {
                    profile: CodexProfile::from_registry(alias.clone(), home),
                    alias,
                });
            }
            _ => entries.push(RegistryEntry::Invalid { alias }),
        }
    }
    Registry {
        entries,
        error: overflow.then_some("Too many custom profiles configured".into()),
    }
}

pub(crate) async fn fetch_from_config(
    config_dir: &Path,
    default_home: Option<&Path>,
) -> CodexProfilesResponse {
    let registry = load_registry(config_dir, default_home);
    let mut profiles = Vec::with_capacity(registry.entries.len());
    for entry in registry.entries {
        match entry {
            RegistryEntry::Valid { alias, profile } => {
                profiles.push(codex::fetch_public_profile(alias, profile).await)
            }
            RegistryEntry::Invalid { alias } => {
                profiles.push(CodexProfilePublicQuota::unavailable(alias))
            }
        }
    }
    CodexProfilesResponse {
        profiles,
        registry_error: registry.error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("quotabar-p4a-{name}-{}", std::process::id()))
    }
    fn write(dir: &Path, value: serde_json::Value) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(CONFIG_FILE), value.to_string()).unwrap();
    }
    fn entry(alias: &str, home: &Path) -> serde_json::Value {
        serde_json::json!({ "alias": alias, "home": home })
    }

    #[test]
    fn missing_config_is_empty_and_default_is_unaffected() {
        let registry = load_registry(&temp("missing"), None);
        assert!(registry.entries.is_empty());
        assert!(registry.error.is_none());
    }

    #[test]
    fn invalid_schema_or_version_is_sanitized() {
        let dir = temp("schema");
        fs::create_dir_all(&dir).unwrap();
        for raw in [
            "{",
            r#"{"version":1,"profiles":[],"extra":true}"#,
            r#"{"version":2,"profiles":[]}"#,
        ] {
            fs::write(dir.join(CONFIG_FILE), raw).unwrap();
            assert_eq!(
                load_registry(&dir, None).error.as_deref(),
                Some("Profile configuration is invalid")
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn aliases_must_be_bounded_safe_unique_and_not_default() {
        assert_eq!(safe_alias("work"), Some("work".into()));
        assert_eq!(safe_alias("default"), None);
        assert_eq!(safe_alias("Default"), None);
        assert_eq!(safe_alias("DEFAULT"), None);
        assert_eq!(safe_alias("contains/path"), None);
        assert_eq!(safe_alias(&"a".repeat(49)), None);
    }

    #[test]
    fn malformed_rows_stay_in_place_without_suppressing_valid_neighbors() {
        let dir = temp("malformed-rows");
        let first = dir.join("first");
        let last = dir.join("last");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&last).unwrap();
        write(
            &dir,
            serde_json::json!({ "version": 1, "profiles": [
                entry("first", &first),
                { "alias": "missing-home" },
                { "alias": 42, "home": &last },
                { "alias": "unknown-field", "home": &last, "extra": true },
                entry("last", &last)
            ]}),
        );
        let registry = load_registry(&dir, None);
        assert!(registry.error.is_none());
        assert!(
            matches!(&registry.entries[0], RegistryEntry::Valid { alias, .. } if alias == "first")
        );
        assert!(matches!(
            &registry.entries[1],
            RegistryEntry::Invalid { .. }
        ));
        assert!(matches!(
            &registry.entries[2],
            RegistryEntry::Invalid { .. }
        ));
        assert!(matches!(
            &registry.entries[3],
            RegistryEntry::Invalid { .. }
        ));
        assert!(
            matches!(&registry.entries[4], RegistryEntry::Valid { alias, .. } if alias == "last")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn generated_invalid_aliases_never_collide_with_declared_valid_aliases() {
        let dir = temp("invalid-aliases");
        let home = dir.join("home");
        fs::create_dir_all(&home).unwrap();
        write(
            &dir,
            serde_json::json!({ "version": 1, "profiles": [
                { "alias": "broken" },
                entry("profile-1", &home)
            ]}),
        );
        let registry = load_registry(&dir, None);
        assert!(
            matches!(&registry.entries[0], RegistryEntry::Invalid { alias } if alias != "profile-1")
        );
        assert!(
            matches!(&registry.entries[1], RegistryEntry::Valid { alias, .. } if alias == "profile-1")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_homes_do_not_suppress_valid_entries_or_order() {
        let dir = temp("homes");
        let good = dir.join("good");
        fs::create_dir_all(&good).unwrap();
        write(
            &dir,
            serde_json::json!({ "version": 1, "profiles": [
                entry("relative", Path::new("relative")),
                entry("parent", Path::new("/tmp/../missing")),
                entry("missing", &dir.join("missing")),
                entry("good", &good)
            ]}),
        );
        let registry = load_registry(&dir, None);
        assert!(matches!(registry.entries[0], RegistryEntry::Invalid { .. }));
        assert!(matches!(registry.entries[1], RegistryEntry::Invalid { .. }));
        assert!(matches!(registry.entries[2], RegistryEntry::Invalid { .. }));
        assert!(
            matches!(&registry.entries[3], RegistryEntry::Valid { alias, .. } if alias == "good")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn canonical_home_must_be_a_directory() {
        let dir = temp("home-file");
        let file = dir.join("auth-file");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&file, "not a home").unwrap();
        write(
            &dir,
            serde_json::json!({ "version": 1, "profiles": [entry("file", &file)] }),
        );
        let registry = load_registry(&dir, None);
        assert!(matches!(
            &registry.entries[0],
            RegistryEntry::Invalid { .. }
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn aliases_and_canonical_routes_cannot_collide() {
        let dir = temp("routes");
        let default = dir.join("default");
        let first = dir.join("first");
        let last = dir.join("last");
        fs::create_dir_all(&default).unwrap();
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&last).unwrap();
        write(
            &dir,
            serde_json::json!({ "version": 1, "profiles": [
                entry("first", &first), entry("first", &last), entry("route-copy", &first),
                entry("default-copy", &default), entry("last", &last)
            ]}),
        );
        let registry = load_registry(&dir, Some(&default));
        assert!(
            matches!(&registry.entries[0], RegistryEntry::Valid { alias, .. } if alias == "first")
        );
        assert!(matches!(registry.entries[1], RegistryEntry::Invalid { .. }));
        assert!(matches!(registry.entries[2], RegistryEntry::Invalid { .. }));
        assert!(matches!(registry.entries[3], RegistryEntry::Invalid { .. }));
        assert!(
            matches!(&registry.entries[4], RegistryEntry::Valid { alias, .. } if alias == "last")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reload_observes_additions_removals_and_declared_order() {
        let dir = temp("reload");
        let a = dir.join("a");
        let b = dir.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        write(
            &dir,
            serde_json::json!({ "version": 1, "profiles": [entry("a", &a)] }),
        );
        assert_eq!(load_registry(&dir, None).entries.len(), 1);
        write(
            &dir,
            serde_json::json!({ "version": 1, "profiles": [entry("b", &b), entry("a", &a)] }),
        );
        let registry = load_registry(&dir, None);
        assert!(matches!(&registry.entries[0], RegistryEntry::Valid { alias, .. } if alias == "b"));
        assert!(matches!(&registry.entries[1], RegistryEntry::Valid { alias, .. } if alias == "a"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cap_is_reported_without_silently_expanding_refresh_work() {
        let dir = temp("cap");
        let mut profiles = Vec::new();
        for index in 0..=MAX_CUSTOM_PROFILES {
            let home = dir.join("homes").join(index.to_string());
            fs::create_dir_all(&home).unwrap();
            profiles.push(entry(&format!("p{index}"), &home));
        }
        write(
            &dir,
            serde_json::json!({ "version": 1, "profiles": profiles }),
        );
        let registry = load_registry(&dir, None);
        assert_eq!(registry.entries.len(), MAX_CUSTOM_PROFILES);
        assert_eq!(
            registry.error.as_deref(),
            Some("Too many custom profiles configured")
        );
        let _ = fs::remove_dir_all(dir);
    }
}

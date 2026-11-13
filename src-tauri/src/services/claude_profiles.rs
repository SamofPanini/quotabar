//! Registry paths are deliberately kept byte-for-byte: Claude uses the path
//! string, rather than its resolved target, when deriving its keychain name.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path};

const CONFIG_FILE: &str = "claude-profiles.json";
const MAX_PROFILES: usize = 6;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub(crate) alias: String,
    pub(crate) config_dir: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Registry {
    version: u32,
    pub(crate) profiles: Vec<Profile>,
    #[serde(default)]
    pub(crate) labels: HashMap<String, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicProfile {
    pub(crate) alias: String,
    pub(crate) label: String,
    pub(crate) status: &'static str,
    pub(crate) account: Option<crate::domain::models::ClaudeAccount>,
    pub(crate) quota: Option<crate::domain::models::QuotaData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expected_service_name: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProfilesResponse {
    pub(crate) profiles: Vec<PublicProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) registry_error: Option<String>,
}

pub(crate) async fn fetch_profiles() -> ProfilesResponse {
    let registry = match load() {
        Ok(registry) => registry,
        Err(_) => {
            return ProfilesResponse {
                profiles: vec![],
                registry_error: Some(
                    "Claude profile configuration is invalid or unavailable".into(),
                ),
            }
        }
    };
    let mut profiles = Vec::new();
    for profile in &registry.profiles {
        profiles.push(super::claude::fetch_profile(profile, &registry.labels).await);
    }
    ProfilesResponse {
        profiles,
        registry_error: None,
    }
}

pub(crate) fn resolve(alias: &str) -> Option<Profile> {
    load()
        .ok()?
        .profiles
        .into_iter()
        .find(|profile| profile.alias == alias && directory_enabled(profile))
}

fn valid_alias(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && alias != "default"
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

fn parse_registry(content: &str, home: &Path) -> Result<Registry, &'static str> {
    let registry: Registry = serde_json::from_str(content).map_err(|_| "invalid")?;
    if registry.version != 1 || registry.profiles.len() > MAX_PROFILES {
        return Err("invalid");
    }
    if registry.labels.keys().any(|key| {
        key.len() != 6
            || !key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err("invalid");
    }
    let mut aliases = HashSet::new();
    let mut paths = HashSet::new();
    let default_dir = home.join(".claude");
    for profile in &registry.profiles {
        let path = Path::new(&profile.config_dir);
        if !valid_alias(&profile.alias)
            || !aliases.insert(&profile.alias)
            || !profile.config_dir.is_ascii()
            || !path.is_absolute()
            || profile.config_dir.ends_with('/')
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
            || path == default_dir
            || !paths.insert(&profile.config_dir)
        {
            return Err("invalid");
        }
    }
    Ok(registry)
}

pub(crate) fn load_at(directory: &Path, home: &Path) -> Result<Registry, &'static str> {
    let path = directory.join(CONFIG_FILE);
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err("unavailable"),
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err("unavailable"),
        }
    }
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Registry {
            version: 1,
            profiles: Vec::new(),
            labels: HashMap::new(),
        }),
        Ok(metadata) if metadata.file_type().is_file() => {
            let content = fs::read_to_string(path).map_err(|_| "unavailable")?;
            parse_registry(&content, home)
        }
        _ => Err("unavailable"),
    }
}

pub(crate) fn load() -> Result<Registry, &'static str> {
    let directory = super::state_location::primary_state_dir().map_err(|_| "unavailable")?;
    let home = dirs::home_dir().ok_or("unavailable")?;
    load_at(&directory, &home)
}

pub(crate) fn directory_enabled(profile: &Profile) -> bool {
    fs::metadata(&profile.config_dir)
        .map(|m| m.is_dir())
        .unwrap_or(false)
}

pub(crate) fn credential_service_name(config_dir: &str) -> String {
    // From Claude CLI 2.1.322; recheck when the CLI is upgraded.
    let digest = Sha256::digest(config_dir.as_bytes());
    format!(
        "Claude Code-credentials-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_service_name_fixed_vectors() {
        for (path, expected) in [
            ("/Users/sam/.claude-acct-lucky", "2becd79b"),
            ("/Users/sam/.claude-acct-lucky/", "fe978c5b"),
            ("/tmp/qb-test/claude-b", "54af776d"),
        ] {
            assert_eq!(
                credential_service_name(path),
                format!("Claude Code-credentials-{expected}")
            );
        }
    }

    #[test]
    fn registry_rejects_bad_paths_and_preserves_valid_path() {
        let home = Path::new("/home/example");
        for path in [
            "relative",
            "/tmp/../x",
            "/tmp/x/",
            "/tmp/é",
            "/home/example/.claude",
        ] {
            let input =
                serde_json::json!({"version": 1, "profiles": [{"alias": "x", "config_dir": path}]})
                    .to_string();
            assert!(parse_registry(&input, home).is_err());
        }
        let input = r#"{"version":1,"profiles":[{"alias":"lucky","config_dir":"/tmp/a//b"}]}"#;
        assert_eq!(
            parse_registry(input, home).unwrap().profiles[0].config_dir,
            "/tmp/a//b"
        );
    }

    #[test]
    fn registry_rejects_duplicate_aliases_and_paths() {
        let home = Path::new("/home/example");
        for entries in [
            serde_json::json!([{"alias":"x","config_dir":"/x"},{"alias":"x","config_dir":"/y"}]),
            serde_json::json!([{"alias":"x","config_dir":"/x"},{"alias":"y","config_dir":"/x"}]),
        ] {
            let input = serde_json::json!({"version":1,"profiles":entries}).to_string();
            assert!(parse_registry(&input, home).is_err());
        }
    }

    #[test]
    fn registry_rejects_invalid_version_alias_count_and_labels() {
        let home = Path::new("/home/example");
        for raw in [
            "{",
            r#"{"version":2,"profiles":[]}"#,
            r#"{"version":1}"#,
            r#"{"version":1,"profiles":[{"alias":"default","config_dir":"/a"}]}"#,
            r#"{"version":1,"profiles":[{"alias":"Upper","config_dir":"/a"}]}"#,
            r#"{"version":1,"profiles":[],"labels":{"ABCD00":"x"}}"#,
            r#"{"version":1,"profiles":[],"extra":true}"#,
            r#"{"version":1,"profiles":[{"alias":"x","config_dir":"/a","extra":true}]}"#,
        ] {
            assert!(parse_registry(raw, home).is_err());
        }
        let profiles: Vec<_> = (0..7).map(|index| serde_json::json!({"alias": format!("p{index}"), "config_dir": format!("/x{index}")})).collect();
        assert!(parse_registry(
            &serde_json::json!({"version": 1, "profiles": profiles}).to_string(),
            home
        )
        .is_err());
    }

    #[test]
    fn registry_non_regular_file_fails_closed() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("quotabar-claude-nonregular-{}", std::process::id()));
        fs::create_dir_all(root.join(CONFIG_FILE)).unwrap();
        assert!(load_at(&root, Path::new("/home/example")).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_registry_and_disabled_directory_are_distinct() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("quotabar-claude-profiles-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let registry = load_at(&root, Path::new("/home/example")).unwrap();
        assert!(registry.profiles.is_empty());
        let profile = Profile {
            alias: "missing".into(),
            config_dir: root.join("missing").to_str().unwrap().into(),
        };
        assert!(!directory_enabled(&profile));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn registry_refuses_symlink_ancestors() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "quotabar-claude-registry-link-{}",
            std::process::id()
        ));
        let real = root.join("real");
        fs::create_dir_all(&real).unwrap();
        symlink(&real, root.join("link")).unwrap();
        assert!(load_at(&root.join("link"), Path::new("/home/example")).is_err());
        let _ = fs::remove_dir_all(root);
    }
}

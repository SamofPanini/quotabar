//! Durable, QuotaBar-owned local state locations.
//!
//! This deliberately avoids Tauri's bundle-identifier-derived app-config
//! location.  The public provenance enum is path-free by construction.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub(crate) const CUSTOM_STATE_DIR: &str = "quotabar-custom";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StateProvenance {
    None,
    Primary,
    Legacy,
}

pub(crate) fn primary_state_dir() -> Result<PathBuf, ()> {
    let home = dirs::home_dir().ok_or(())?;
    Ok(primary_state_dir_from_home(&home))
}

fn primary_state_dir_from_home(home: &Path) -> PathBuf {
    home.join(".config").join(CUSTOM_STATE_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_home_fixture_resolves_a_custom_non_bundle_owned_path() {
        let root = primary_state_dir_from_home(Path::new("/isolated-home"));
        assert_eq!(root, Path::new("/isolated-home/.config/quotabar-custom"));
        assert!(!root.to_string_lossy().contains("com.majiayu.quotabar"));
    }
}

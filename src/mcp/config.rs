#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    env,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpProfile {
    Default,
    Compiler,
}

#[derive(Debug, Clone)]
pub struct McpConfig {
    pub base_url: String,
    pub default_tenant: String,
    pub expose_embeddings: bool,
    pub profile: McpProfile,
    pub compiler_id: String,
}

impl McpConfig {
    pub fn from_env(profile: McpProfile) -> Self {
        let base_url = env::var("MEM_BASE_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:3000".to_string());

        let default_tenant = env::var("MEM_TENANT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "local".to_string());

        let expose_embeddings = matches!(env::var("MEM_MCP_EXPOSE_EMBEDDINGS").as_deref(), Ok("1"));
        let compiler_id = env::var("MEM_AGENT_COMPILER_ID")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 128
                    && value.bytes().all(|byte| byte.is_ascii_graphic())
            })
            .unwrap_or_else(|| "agent-mcp".to_string());

        Self {
            base_url,
            default_tenant,
            expose_embeddings,
            profile,
            compiler_id,
        }
    }
}

pub fn role_token(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| local_config_value(key))
}

fn local_config_value(key: &str) -> Option<String> {
    let path = env::var_os("MEM_CONFIG_ENV")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".mem/config.env")))?;
    local_config_value_from_path(&path, key)
}

fn local_config_value_from_path(path: &Path, key: &str) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > 128 * 1024
        || !secure_owner_and_mode(&metadata, false)
    {
        return None;
    }
    let parent = path.parent()?;
    let parent_metadata = std::fs::symlink_metadata(parent).ok()?;
    if parent_metadata.file_type().is_symlink()
        || !parent_metadata.is_dir()
        || !secure_owner_and_mode(&parent_metadata, true)
    {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    content.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (candidate, value) = line.split_once('=')?;
        if candidate.trim() != key {
            return None;
        }
        let value = value.trim();
        (!value.is_empty() && !value.chars().any(char::is_control)).then(|| value.to_string())
    })
}

#[cfg(unix)]
fn secure_owner_and_mode(metadata: &std::fs::Metadata, directory: bool) -> bool {
    let expected_mode = if directory { 0o700 } else { 0o600 };
    // SAFETY: geteuid has no arguments and only reads the process credential.
    metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == expected_mode
}

#[cfg(not(unix))]
fn secure_owner_and_mode(_metadata: &std::fs::Metadata, _directory: bool) -> bool {
    // Without a platform ACL/owner check, silently reading a role token from
    // disk would weaken the compiler/reviewer process boundary. Non-Unix
    // callers can still provide role tokens explicitly through the process
    // environment; the local-file fallback stays fail-closed.
    false
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::{symlink, PermissionsExt};

    use tempfile::tempdir;

    use super::*;

    fn write_secure_config() -> (tempfile::TempDir, PathBuf) {
        let root = tempdir().expect("temp dir");
        let config_dir = root.path().join(".mem");
        std::fs::create_dir(&config_dir).expect("config dir");
        std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700))
            .expect("secure config dir");
        let path = config_dir.join("config.env");
        std::fs::write(&path, "MEM_SKILL_COMPILER_TOKEN=compiler-secret\n").expect("write config");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("secure config file");
        (root, path)
    }

    #[test]
    fn local_role_config_requires_private_owned_path() {
        let (_root, path) = write_secure_config();
        assert_eq!(
            local_config_value_from_path(&path, "MEM_SKILL_COMPILER_TOKEN").as_deref(),
            Some("compiler-secret")
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .expect("weaken config permissions");
        assert!(local_config_value_from_path(&path, "MEM_SKILL_COMPILER_TOKEN").is_none());
    }

    #[test]
    fn local_role_config_rejects_symlink_and_public_parent() {
        let (_root, path) = write_secure_config();
        let link = path.with_file_name("linked.env");
        symlink(&path, &link).expect("config symlink");
        assert!(local_config_value_from_path(&link, "MEM_SKILL_COMPILER_TOKEN").is_none());

        let parent = path.parent().expect("config parent");
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755))
            .expect("weaken parent permissions");
        assert!(local_config_value_from_path(&path, "MEM_SKILL_COMPILER_TOKEN").is_none());
    }
}

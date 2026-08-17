use std::{collections::HashMap, fmt};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

pub const SKILL_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const SKILL_DOCUMENT_PATH: &str = "SKILL.md";
pub const MAX_RESOURCE_FILES: usize = 64;
pub const MAX_RESOURCE_PATH_BYTES: usize = 256;
pub const MAX_RESOURCE_SEGMENT_BYTES: usize = 128;
pub const MAX_SINGLE_RESOURCE_BYTES: u64 = 1024 * 1024;
pub const MAX_BUNDLE_BYTES: u64 = 4 * 1024 * 1024;

const ALLOWED_TEXT_MEDIA_TYPES: &[&str] = &[
    "text/markdown",
    "text/plain",
    "application/json",
    "application/yaml",
    "application/x-yaml",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct SkillId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundleVersion {
    pub bundle_version_id: String,
    pub skill_id: SkillId,
    pub previous_bundle_version_id: Option<String>,
    pub manifest: SkillManifest,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillManifest {
    pub schema_version: u32,
    pub skill_id: SkillId,
    pub resources: Vec<ResourceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceEntry {
    pub path: String,
    pub media_type: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillBundleError {
    UnsupportedSchemaVersion {
        actual: u32,
    },
    EmptySkillId,
    EmptyBundleVersionId,
    MissingSkillDocument,
    MultipleSkillDocuments,
    TooManyResources {
        count: usize,
        max: usize,
    },
    InvalidResourcePath {
        path: String,
        reason: &'static str,
    },
    ResourcePathTooLong {
        path: String,
        max_bytes: usize,
    },
    ResourceSegmentTooLong {
        path: String,
        max_bytes: usize,
    },
    DuplicateResourcePath {
        first: String,
        duplicate: String,
    },
    UnsupportedResourceMediaType {
        path: String,
        media_type: String,
    },
    InvalidResourceSha256 {
        path: String,
    },
    ResourceTooLarge {
        path: String,
        size_bytes: u64,
        max_bytes: u64,
    },
    BundleTooLarge {
        size_bytes: u64,
        max_bytes: u64,
    },
    ExecutableResource {
        path: String,
    },
    InvalidManifestSha256,
    ManifestSkillIdMismatch,
    ManifestDigestMismatch,
    CanonicalSerialization(String),
}

impl fmt::Display for SkillBundleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid skill bundle: {self:?}")
    }
}

impl std::error::Error for SkillBundleError {}

impl SkillManifest {
    pub fn validate(&self) -> Result<(), SkillBundleError> {
        if self.schema_version != SKILL_MANIFEST_SCHEMA_VERSION {
            return Err(SkillBundleError::UnsupportedSchemaVersion {
                actual: self.schema_version,
            });
        }
        if self.skill_id.0.trim().is_empty() {
            return Err(SkillBundleError::EmptySkillId);
        }
        if self.resources.len() > MAX_RESOURCE_FILES {
            return Err(SkillBundleError::TooManyResources {
                count: self.resources.len(),
                max: MAX_RESOURCE_FILES,
            });
        }

        match self
            .resources
            .iter()
            .filter(|resource| resource.path == SKILL_DOCUMENT_PATH)
            .count()
        {
            0 => return Err(SkillBundleError::MissingSkillDocument),
            1 => {}
            _ => return Err(SkillBundleError::MultipleSkillDocuments),
        }

        let mut case_folded_paths: HashMap<String, &str> = HashMap::new();
        let mut total_size = 0_u64;
        for resource in &self.resources {
            validate_resource_path(&resource.path)?;
            let folded = case_fold_path(&resource.path);
            if let Some(first) = case_folded_paths.insert(folded, &resource.path) {
                return Err(SkillBundleError::DuplicateResourcePath {
                    first: first.to_string(),
                    duplicate: resource.path.clone(),
                });
            }
            if !is_allowed_text_media_type(&resource.media_type) {
                return Err(SkillBundleError::UnsupportedResourceMediaType {
                    path: resource.path.clone(),
                    media_type: resource.media_type.clone(),
                });
            }
            if !is_lowercase_sha256(&resource.sha256) {
                return Err(SkillBundleError::InvalidResourceSha256 {
                    path: resource.path.clone(),
                });
            }
            if resource.size_bytes > MAX_SINGLE_RESOURCE_BYTES {
                return Err(SkillBundleError::ResourceTooLarge {
                    path: resource.path.clone(),
                    size_bytes: resource.size_bytes,
                    max_bytes: MAX_SINGLE_RESOURCE_BYTES,
                });
            }
            if resource.executable {
                return Err(SkillBundleError::ExecutableResource {
                    path: resource.path.clone(),
                });
            }
            total_size = total_size.checked_add(resource.size_bytes).ok_or(
                SkillBundleError::BundleTooLarge {
                    size_bytes: u64::MAX,
                    max_bytes: MAX_BUNDLE_BYTES,
                },
            )?;
        }
        if total_size > MAX_BUNDLE_BYTES {
            return Err(SkillBundleError::BundleTooLarge {
                size_bytes: total_size,
                max_bytes: MAX_BUNDLE_BYTES,
            });
        }
        Ok(())
    }

    pub fn canonical_json(&self) -> Result<String, SkillBundleError> {
        self.validate()?;
        let mut resources: Vec<&ResourceEntry> = self.resources.iter().collect();
        resources.sort_by(|left, right| left.path.cmp(&right.path));
        let canonical = CanonicalManifest {
            schema_version: self.schema_version,
            skill_id: &self.skill_id.0,
            resources: resources
                .into_iter()
                .map(CanonicalResourceEntry::from)
                .collect(),
        };
        serde_json::to_string(&canonical)
            .map_err(|error| SkillBundleError::CanonicalSerialization(error.to_string()))
    }

    pub fn digest(&self) -> Result<String, SkillBundleError> {
        let canonical = self.canonical_json()?;
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        Ok(format!("{:x}", hasher.finalize()))
    }
}

impl BundleVersion {
    pub fn validate(&self) -> Result<(), SkillBundleError> {
        if self.bundle_version_id.trim().is_empty() {
            return Err(SkillBundleError::EmptyBundleVersionId);
        }
        if self.skill_id.0.trim().is_empty() {
            return Err(SkillBundleError::EmptySkillId);
        }
        self.manifest.validate()?;
        if self.skill_id != self.manifest.skill_id {
            return Err(SkillBundleError::ManifestSkillIdMismatch);
        }
        if !is_lowercase_sha256(&self.manifest_sha256) {
            return Err(SkillBundleError::InvalidManifestSha256);
        }
        if self.manifest.digest()? != self.manifest_sha256 {
            return Err(SkillBundleError::ManifestDigestMismatch);
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct CanonicalManifest<'a> {
    schema_version: u32,
    skill_id: &'a str,
    resources: Vec<CanonicalResourceEntry<'a>>,
}

#[derive(Serialize)]
struct CanonicalResourceEntry<'a> {
    path: &'a str,
    media_type: &'a str,
    sha256: &'a str,
    size_bytes: u64,
    executable: bool,
}

impl<'a> From<&'a ResourceEntry> for CanonicalResourceEntry<'a> {
    fn from(resource: &'a ResourceEntry) -> Self {
        Self {
            path: &resource.path,
            media_type: &resource.media_type,
            sha256: &resource.sha256,
            size_bytes: resource.size_bytes,
            executable: resource.executable,
        }
    }
}

fn validate_resource_path(path: &str) -> Result<(), SkillBundleError> {
    if path.is_empty() {
        return Err(invalid_path(path, "path is empty"));
    }
    if path.len() > MAX_RESOURCE_PATH_BYTES {
        return Err(SkillBundleError::ResourcePathTooLong {
            path: path.to_string(),
            max_bytes: MAX_RESOURCE_PATH_BYTES,
        });
    }
    if path.starts_with('/') {
        return Err(invalid_path(path, "path must be relative"));
    }
    if path.contains('\\') {
        return Err(invalid_path(path, "backslashes are not allowed"));
    }
    if path.contains(':') {
        return Err(invalid_path(path, "colons are not allowed"));
    }
    if path.chars().any(char::is_control) {
        return Err(invalid_path(path, "control characters are not allowed"));
    }
    if !path.nfc().eq(path.chars()) {
        return Err(invalid_path(path, "path must be NFC normalized"));
    }
    for segment in path.split('/') {
        if segment.is_empty() {
            return Err(invalid_path(path, "empty path segments are not allowed"));
        }
        if segment == "." || segment == ".." {
            return Err(invalid_path(path, "dot path segments are not allowed"));
        }
        if segment.ends_with('.') || segment.ends_with(' ') {
            return Err(invalid_path(
                path,
                "path segments cannot end with a dot or space",
            ));
        }
        if segment.len() > MAX_RESOURCE_SEGMENT_BYTES {
            return Err(SkillBundleError::ResourceSegmentTooLong {
                path: path.to_string(),
                max_bytes: MAX_RESOURCE_SEGMENT_BYTES,
            });
        }
    }
    Ok(())
}

pub(crate) fn is_allowed_text_media_type(media_type: &str) -> bool {
    ALLOWED_TEXT_MEDIA_TYPES.contains(&media_type)
}

fn invalid_path(path: &str, reason: &'static str) -> SkillBundleError {
    SkillBundleError::InvalidResourcePath {
        path: path.to_string(),
        reason,
    }
}

fn case_fold_path(path: &str) -> String {
    path.chars()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .nfc()
        .collect()
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(path: impl Into<String>, size_bytes: u64) -> ResourceEntry {
        ResourceEntry {
            path: path.into(),
            media_type: "text/markdown".to_string(),
            sha256: "0".repeat(64),
            size_bytes,
            executable: false,
        }
    }

    fn manifest(resources: Vec<ResourceEntry>) -> SkillManifest {
        SkillManifest {
            schema_version: SKILL_MANIFEST_SCHEMA_VERSION,
            skill_id: SkillId("skill-demo".to_string()),
            resources,
        }
    }

    #[test]
    fn valid_manifest_has_stable_canonical_resource_order() {
        let manifest = manifest(vec![
            resource("references/guide.md", 64),
            resource(SKILL_DOCUMENT_PATH, 128),
        ]);

        assert_eq!(
            manifest.canonical_json().unwrap(),
            concat!(
                "{\"schema_version\":1,\"skill_id\":\"skill-demo\",\"resources\":[",
                "{\"path\":\"SKILL.md\",\"media_type\":\"text/markdown\",",
                "\"sha256\":\"0000000000000000000000000000000000000000000000000000000000000000\",",
                "\"size_bytes\":128,\"executable\":false},",
                "{\"path\":\"references/guide.md\",\"media_type\":\"text/markdown\",",
                "\"sha256\":\"0000000000000000000000000000000000000000000000000000000000000000\",",
                "\"size_bytes\":64,\"executable\":false}]}",
            )
        );
    }

    #[test]
    fn canonical_manifest_digest_is_stable() {
        let manifest = manifest(vec![resource(SKILL_DOCUMENT_PATH, 128)]);

        assert_eq!(
            manifest.digest().unwrap(),
            "ac1188a5b1f54cc9634be00bb78e7081e8d65d80088bb938b6acf3794d4ee421".to_string()
        );
    }

    #[test]
    fn manifest_requires_exactly_one_skill_document() {
        let missing = manifest(vec![resource("references/guide.md", 64)]);
        assert_eq!(
            missing.validate(),
            Err(SkillBundleError::MissingSkillDocument)
        );

        let repeated = manifest(vec![
            resource(SKILL_DOCUMENT_PATH, 128),
            resource(SKILL_DOCUMENT_PATH, 128),
        ]);
        assert_eq!(
            repeated.validate(),
            Err(SkillBundleError::MultipleSkillDocuments)
        );
    }

    #[test]
    fn resource_paths_reject_traversal_non_posix_and_non_nfc_forms() {
        for invalid in [
            "/SKILL.md",
            "../SKILL.md",
            "references/../SKILL.md",
            "references\\guide.md",
            "references//guide.md",
            "references/./guide.md",
            "references/guide.md/",
            "references/\0guide.md",
            "references/e\u{301}.md",
        ] {
            let manifest = manifest(vec![
                resource(SKILL_DOCUMENT_PATH, 128),
                resource(invalid, 64),
            ]);
            assert!(
                matches!(
                    manifest.validate(),
                    Err(SkillBundleError::InvalidResourcePath { .. })
                ),
                "path should be rejected: {invalid:?}"
            );
        }
    }

    #[test]
    fn resource_paths_are_unique_after_case_folding() {
        let manifest = manifest(vec![
            resource(SKILL_DOCUMENT_PATH, 128),
            resource("references/Guide.md", 64),
            resource("references/guide.md", 64),
        ]);

        assert!(matches!(
            manifest.validate(),
            Err(SkillBundleError::DuplicateResourcePath { .. })
        ));
    }

    #[test]
    fn resource_descriptors_reject_hash_mime_and_executable_violations() {
        let mut uppercase_hash = resource(SKILL_DOCUMENT_PATH, 128);
        uppercase_hash.sha256 = "A".repeat(64);
        assert!(matches!(
            manifest(vec![uppercase_hash]).validate(),
            Err(SkillBundleError::InvalidResourceSha256 { .. })
        ));

        let mut binary = resource(SKILL_DOCUMENT_PATH, 128);
        binary.media_type = "application/octet-stream".to_string();
        assert!(matches!(
            manifest(vec![binary]).validate(),
            Err(SkillBundleError::UnsupportedResourceMediaType { .. })
        ));

        let mut executable = resource(SKILL_DOCUMENT_PATH, 128);
        executable.executable = true;
        assert!(matches!(
            manifest(vec![executable]).validate(),
            Err(SkillBundleError::ExecutableResource { .. })
        ));
    }

    #[test]
    fn manifest_enforces_file_path_single_file_and_total_size_limits() {
        let mut too_many = vec![resource(SKILL_DOCUMENT_PATH, 1)];
        too_many.extend(
            (0..MAX_RESOURCE_FILES).map(|index| resource(format!("references/{index}.md"), 1)),
        );
        assert_eq!(
            manifest(too_many).validate(),
            Err(SkillBundleError::TooManyResources {
                count: MAX_RESOURCE_FILES + 1,
                max: MAX_RESOURCE_FILES,
            })
        );

        let long_path = format!("{}.md", "a".repeat(MAX_RESOURCE_PATH_BYTES));
        assert!(matches!(
            manifest(vec![
                resource(SKILL_DOCUMENT_PATH, 1),
                resource(long_path, 1)
            ])
            .validate(),
            Err(SkillBundleError::ResourcePathTooLong { .. })
        ));

        let long_segment = format!("references/{}.md", "a".repeat(MAX_RESOURCE_SEGMENT_BYTES));
        assert!(matches!(
            manifest(vec![
                resource(SKILL_DOCUMENT_PATH, 1),
                resource(long_segment, 1)
            ])
            .validate(),
            Err(SkillBundleError::ResourceSegmentTooLong { .. })
        ));

        assert!(matches!(
            manifest(vec![resource(
                SKILL_DOCUMENT_PATH,
                MAX_SINGLE_RESOURCE_BYTES + 1
            )])
            .validate(),
            Err(SkillBundleError::ResourceTooLarge { .. })
        ));

        let total = manifest(vec![
            resource(SKILL_DOCUMENT_PATH, MAX_SINGLE_RESOURCE_BYTES),
            resource("references/1.md", MAX_SINGLE_RESOURCE_BYTES),
            resource("references/2.md", MAX_SINGLE_RESOURCE_BYTES),
            resource("references/3.md", MAX_SINGLE_RESOURCE_BYTES),
            resource("references/4.md", 1),
        ]);
        assert!(matches!(
            total.validate(),
            Err(SkillBundleError::BundleTooLarge { .. })
        ));
    }

    #[test]
    fn bundle_version_binds_ids_and_manifest_digest() {
        let manifest = manifest(vec![resource(SKILL_DOCUMENT_PATH, 128)]);
        let digest = manifest.digest().unwrap();
        let version = BundleVersion {
            bundle_version_id: "bundle-v1".to_string(),
            skill_id: SkillId("skill-demo".to_string()),
            previous_bundle_version_id: None,
            manifest,
            manifest_sha256: digest,
        };
        assert_eq!(version.validate(), Ok(()));

        let mut wrong_digest = version.clone();
        wrong_digest.manifest_sha256 = "f".repeat(64);
        assert_eq!(
            wrong_digest.validate(),
            Err(SkillBundleError::ManifestDigestMismatch)
        );
    }
}

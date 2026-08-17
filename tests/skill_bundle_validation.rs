use mem::{
    domain::{
        ResourceEntry, SkillId, SkillManifest, SkillResourceBlob, SKILL_DOCUMENT_PATH,
        SKILL_MANIFEST_SCHEMA_VERSION,
    },
    storage::{SkillStore, Store},
};
use sha2::{Digest, Sha256};

const TENANT: &str = "local";
const NOW: &str = "00000001786000000000";

fn resource(path: &str) -> ResourceEntry {
    ResourceEntry {
        path: path.to_owned(),
        media_type: "text/markdown".to_owned(),
        sha256: "0".repeat(64),
        size_bytes: 0,
        executable: false,
    }
}

fn manifest_with(path: &str) -> SkillManifest {
    SkillManifest {
        schema_version: SKILL_MANIFEST_SCHEMA_VERSION,
        skill_id: SkillId("skill-path-validation".to_owned()),
        resources: vec![resource(SKILL_DOCUMENT_PATH), resource(path)],
    }
}

fn blob(content: Vec<u8>) -> SkillResourceBlob {
    SkillResourceBlob {
        tenant: TENANT.to_owned(),
        sha256: format!("{:x}", Sha256::digest(&content)),
        media_type: "text/plain".to_owned(),
        size_bytes: content.len() as u64,
        content,
        created_at: NOW.to_owned(),
    }
}

#[test]
fn bundle_paths_reject_windows_drives_ads_colons_and_trailing_dot_or_space() {
    for path in [
        "C:/escape.md",
        "notes:stream.md",
        "notes.md:stream",
        "notes.md.",
        "notes.md ",
    ] {
        assert!(
            manifest_with(path).validate().is_err(),
            "unsafe cross-platform path was accepted: {path:?}",
        );
    }
}

#[tokio::test]
async fn text_resource_blobs_reject_non_utf8_and_nul_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("skill-text-validation.lance"))
        .await
        .expect("Store::open");

    for content in [
        vec![0xff, 0xfe, 0xfd],
        b"valid prefix\0hidden suffix".to_vec(),
    ] {
        assert!(
            store.put_skill_resource_blob(blob(content)).await.is_err(),
            "invalid text blob must be rejected",
        );
    }
}

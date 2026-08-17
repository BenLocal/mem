use super::*;

#[tokio::test]
async fn heads_and_pins_are_tenant_isolated() {
    let dir = tempdir().unwrap();
    let store = Store::open(dir.path().join("runtime.lance")).await.unwrap();
    for (tenant, version) in [("tenant-a", "v-a"), ("tenant-b", "v-b")] {
        let proposal_id = format!("proposal-{tenant}");
        let bundle = accepted_bundle(
            &store,
            tenant,
            &proposal_id,
            "shared-skill-id",
            version,
            b"Example instructions",
        )
        .await;
        store.append_skill_bundle_version(bundle).await.unwrap();
        store
            .compare_and_set_skill_head(
                None,
                SkillHead {
                    tenant: tenant.to_string(),
                    skill_id: "shared-skill-id".to_string(),
                    bundle_version_id: version.to_string(),
                    updated_at: NOW.to_string(),
                },
            )
            .await
            .unwrap();
        store
            .get_or_pin_session_skill(SessionSkillPin {
                tenant: tenant.to_string(),
                session_id: "same-session".to_string(),
                agent_id: "same-agent".to_string(),
                skill_id: "shared-skill-id".to_string(),
                bundle_version_id: version.to_string(),
                pinned_at: NOW.to_string(),
                expires_at: "00000001787000001000".to_string(),
                revision: 1,
            })
            .await
            .unwrap();
    }

    assert_eq!(
        store
            .get_skill_head("tenant-a", "shared-skill-id")
            .await
            .unwrap()
            .unwrap()
            .bundle_version_id,
        "v-a"
    );
    assert_eq!(
        store
            .get_skill_head("tenant-b", "shared-skill-id")
            .await
            .unwrap()
            .unwrap()
            .bundle_version_id,
        "v-b"
    );
}

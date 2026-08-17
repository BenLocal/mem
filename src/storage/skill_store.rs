use async_trait::async_trait;

use crate::domain::{
    AgentLoadoutBinding, SessionSkillPin, SkillBundleRevocation, SkillBundleVersionRecord,
    SkillCompileDecisionRecord, SkillFeedbackEvent, SkillHead, SkillProposalRecord,
    SkillProposalStatus, SkillResourceBlob, SkillRevisionCandidate,
};

use super::{StorageError, Store};

#[async_trait]
pub trait SkillStore: Send + Sync {
    async fn get_skill_proposal(
        &self,
        tenant: &str,
        proposal_id: &str,
    ) -> Result<Option<SkillProposalRecord>, StorageError>;

    async fn get_skill_proposal_by_job(
        &self,
        tenant: &str,
        job_id: &str,
    ) -> Result<Option<SkillProposalRecord>, StorageError>;

    async fn settle_skill_proposal(
        &self,
        proposal: SkillProposalRecord,
    ) -> Result<SkillProposalRecord, StorageError>;

    async fn update_skill_proposal_outcome(
        &self,
        tenant: &str,
        proposal_id: &str,
        expected_status: SkillProposalStatus,
        status: SkillProposalStatus,
        updated_at: &str,
    ) -> Result<SkillProposalRecord, StorageError>;

    async fn put_skill_resource_blob(
        &self,
        blob: SkillResourceBlob,
    ) -> Result<SkillResourceBlob, StorageError>;

    async fn get_skill_resource_blob(
        &self,
        tenant: &str,
        sha256: &str,
    ) -> Result<Option<SkillResourceBlob>, StorageError>;

    async fn append_skill_bundle_version(
        &self,
        bundle: SkillBundleVersionRecord,
    ) -> Result<SkillBundleVersionRecord, StorageError>;

    async fn get_skill_bundle_version(
        &self,
        tenant: &str,
        skill_id: &str,
        bundle_version_id: &str,
    ) -> Result<Option<SkillBundleVersionRecord>, StorageError>;

    async fn find_skill_bundle_by_workflow_capsule(
        &self,
        tenant: &str,
        workflow_capsule_id: &str,
    ) -> Result<Option<SkillBundleVersionRecord>, StorageError>;

    async fn get_skill_head(
        &self,
        tenant: &str,
        skill_id: &str,
    ) -> Result<Option<SkillHead>, StorageError>;

    async fn compare_and_set_skill_head(
        &self,
        expected_version: Option<&str>,
        head: SkillHead,
    ) -> Result<SkillHead, StorageError>;

    async fn bind_agent_loadout(
        &self,
        binding: AgentLoadoutBinding,
    ) -> Result<AgentLoadoutBinding, StorageError>;

    async fn list_agent_loadout(
        &self,
        tenant: &str,
        agent_id: &str,
        limit: usize,
    ) -> Result<Vec<AgentLoadoutBinding>, StorageError>;

    async fn get_agent_loadout_binding(
        &self,
        tenant: &str,
        agent_id: &str,
        skill_id: &str,
    ) -> Result<Option<AgentLoadoutBinding>, StorageError>;

    async fn get_or_pin_session_skill(
        &self,
        pin: SessionSkillPin,
    ) -> Result<SessionSkillPin, StorageError>;

    async fn get_session_skill_pin(
        &self,
        tenant: &str,
        session_id: &str,
        agent_id: &str,
        skill_id: &str,
    ) -> Result<Option<SessionSkillPin>, StorageError>;

    async fn revoke_skill_bundle(
        &self,
        revocation: SkillBundleRevocation,
    ) -> Result<SkillBundleRevocation, StorageError>;

    async fn get_skill_bundle_revocation(
        &self,
        tenant: &str,
        skill_id: &str,
        bundle_version_id: &str,
    ) -> Result<Option<SkillBundleRevocation>, StorageError>;

    async fn insert_skill_revision_candidate(
        &self,
        candidate: SkillRevisionCandidate,
    ) -> Result<SkillRevisionCandidate, StorageError>;

    async fn get_skill_revision_candidate(
        &self,
        tenant: &str,
        job_id: &str,
    ) -> Result<Option<SkillRevisionCandidate>, StorageError>;

    async fn get_skill_compile_decision(
        &self,
        tenant: &str,
        job_id: &str,
    ) -> Result<Option<SkillCompileDecisionRecord>, StorageError>;

    async fn settle_skill_compile_decision(
        &self,
        decision: SkillCompileDecisionRecord,
    ) -> Result<SkillCompileDecisionRecord, StorageError>;

    async fn append_skill_feedback(
        &self,
        feedback: SkillFeedbackEvent,
    ) -> Result<SkillFeedbackEvent, StorageError>;

    async fn list_skill_feedback(
        &self,
        tenant: &str,
        skill_id: &str,
        bundle_version_id: &str,
        limit: usize,
    ) -> Result<Vec<SkillFeedbackEvent>, StorageError>;
}

#[async_trait]
impl SkillStore for Store {
    async fn get_skill_proposal(
        &self,
        tenant: &str,
        proposal_id: &str,
    ) -> Result<Option<SkillProposalRecord>, StorageError> {
        self.lance.get_skill_proposal(tenant, proposal_id).await
    }

    async fn get_skill_proposal_by_job(
        &self,
        tenant: &str,
        job_id: &str,
    ) -> Result<Option<SkillProposalRecord>, StorageError> {
        self.lance.get_skill_proposal_by_job(tenant, job_id).await
    }

    async fn settle_skill_proposal(
        &self,
        proposal: SkillProposalRecord,
    ) -> Result<SkillProposalRecord, StorageError> {
        let _guard = self.skill_runtime_gate.lock().await;
        if self
            .lance
            .get_skill_compile_decision(&proposal.tenant, &proposal.job_id)
            .await?
            .is_some()
        {
            return Err(StorageError::Conflict(
                "Skill compiler job already has a terminal decision",
            ));
        }
        if let Some(existing) = self
            .lance
            .get_skill_proposal_by_job(&proposal.tenant, &proposal.job_id)
            .await?
        {
            if existing.proposal_id != proposal.proposal_id {
                return Err(StorageError::Conflict(
                    "Skill compiler job already has a different proposal",
                ));
            }
        }
        self.commit_lance_write(self.lance.insert_skill_proposal(proposal).await)
            .await
    }

    async fn update_skill_proposal_outcome(
        &self,
        tenant: &str,
        proposal_id: &str,
        expected_status: SkillProposalStatus,
        status: SkillProposalStatus,
        updated_at: &str,
    ) -> Result<SkillProposalRecord, StorageError> {
        let _guard = self.skill_runtime_gate.lock().await;
        self.commit_lance_write(
            self.lance
                .update_skill_proposal_outcome(
                    tenant,
                    proposal_id,
                    expected_status,
                    status,
                    updated_at,
                )
                .await,
        )
        .await
    }

    async fn put_skill_resource_blob(
        &self,
        blob: SkillResourceBlob,
    ) -> Result<SkillResourceBlob, StorageError> {
        let _guard = self.skill_runtime_gate.lock().await;
        self.commit_lance_write(self.lance.put_skill_resource_blob(blob).await)
            .await
    }

    async fn append_skill_bundle_version(
        &self,
        bundle: SkillBundleVersionRecord,
    ) -> Result<SkillBundleVersionRecord, StorageError> {
        let _guard = self.skill_runtime_gate.lock().await;
        self.commit_lance_write(self.lance.append_skill_bundle_version(bundle).await)
            .await
    }

    async fn get_skill_resource_blob(
        &self,
        tenant: &str,
        sha256: &str,
    ) -> Result<Option<SkillResourceBlob>, StorageError> {
        self.lance.get_skill_resource_blob(tenant, sha256).await
    }

    async fn get_skill_bundle_version(
        &self,
        tenant: &str,
        skill_id: &str,
        bundle_version_id: &str,
    ) -> Result<Option<SkillBundleVersionRecord>, StorageError> {
        self.lance
            .get_skill_bundle_version(tenant, skill_id, bundle_version_id)
            .await
    }

    async fn find_skill_bundle_by_workflow_capsule(
        &self,
        tenant: &str,
        workflow_capsule_id: &str,
    ) -> Result<Option<SkillBundleVersionRecord>, StorageError> {
        self.lance
            .find_skill_bundle_by_workflow_capsule(tenant, workflow_capsule_id)
            .await
    }

    async fn get_skill_head(
        &self,
        tenant: &str,
        skill_id: &str,
    ) -> Result<Option<SkillHead>, StorageError> {
        self.lance.get_skill_head(tenant, skill_id).await
    }

    async fn compare_and_set_skill_head(
        &self,
        expected_version: Option<&str>,
        head: SkillHead,
    ) -> Result<SkillHead, StorageError> {
        let _guard = self.skill_runtime_gate.lock().await;
        self.commit_lance_write(
            self.lance
                .compare_and_set_skill_head(expected_version, head)
                .await,
        )
        .await
    }

    async fn bind_agent_loadout(
        &self,
        binding: AgentLoadoutBinding,
    ) -> Result<AgentLoadoutBinding, StorageError> {
        let _guard = self.skill_runtime_gate.lock().await;
        self.commit_lance_write(self.lance.bind_agent_loadout(binding).await)
            .await
    }

    async fn list_agent_loadout(
        &self,
        tenant: &str,
        agent_id: &str,
        limit: usize,
    ) -> Result<Vec<AgentLoadoutBinding>, StorageError> {
        self.lance.list_agent_loadout(tenant, agent_id, limit).await
    }

    async fn get_agent_loadout_binding(
        &self,
        tenant: &str,
        agent_id: &str,
        skill_id: &str,
    ) -> Result<Option<AgentLoadoutBinding>, StorageError> {
        self.lance
            .get_agent_loadout_binding(tenant, agent_id, skill_id)
            .await
    }

    async fn get_or_pin_session_skill(
        &self,
        pin: SessionSkillPin,
    ) -> Result<SessionSkillPin, StorageError> {
        let _guard = self.skill_runtime_gate.lock().await;
        self.commit_lance_write(self.lance.get_or_pin_session_skill(pin).await)
            .await
    }

    async fn get_session_skill_pin(
        &self,
        tenant: &str,
        session_id: &str,
        agent_id: &str,
        skill_id: &str,
    ) -> Result<Option<SessionSkillPin>, StorageError> {
        self.lance
            .get_session_skill_pin(tenant, session_id, agent_id, skill_id)
            .await
    }

    async fn revoke_skill_bundle(
        &self,
        revocation: SkillBundleRevocation,
    ) -> Result<SkillBundleRevocation, StorageError> {
        let _guard = self.skill_runtime_gate.lock().await;
        self.commit_lance_write(self.lance.revoke_skill_bundle(revocation).await)
            .await
    }

    async fn get_skill_bundle_revocation(
        &self,
        tenant: &str,
        skill_id: &str,
        bundle_version_id: &str,
    ) -> Result<Option<SkillBundleRevocation>, StorageError> {
        self.lance
            .get_skill_bundle_revocation(tenant, skill_id, bundle_version_id)
            .await
    }

    async fn insert_skill_revision_candidate(
        &self,
        candidate: SkillRevisionCandidate,
    ) -> Result<SkillRevisionCandidate, StorageError> {
        let _guard = self.skill_runtime_gate.lock().await;
        self.commit_lance_write(self.lance.insert_skill_revision_candidate(candidate).await)
            .await
    }

    async fn get_skill_revision_candidate(
        &self,
        tenant: &str,
        job_id: &str,
    ) -> Result<Option<SkillRevisionCandidate>, StorageError> {
        self.lance
            .get_skill_revision_candidate(tenant, job_id)
            .await
    }

    async fn get_skill_compile_decision(
        &self,
        tenant: &str,
        job_id: &str,
    ) -> Result<Option<SkillCompileDecisionRecord>, StorageError> {
        self.lance.get_skill_compile_decision(tenant, job_id).await
    }

    async fn settle_skill_compile_decision(
        &self,
        decision: SkillCompileDecisionRecord,
    ) -> Result<SkillCompileDecisionRecord, StorageError> {
        let _guard = self.skill_runtime_gate.lock().await;
        if self
            .lance
            .get_skill_proposal_by_job(&decision.tenant, &decision.job_id)
            .await?
            .is_some()
        {
            return Err(StorageError::Conflict(
                "Skill compiler job already has a proposal",
            ));
        }
        self.commit_lance_write(self.lance.insert_skill_compile_decision(decision).await)
            .await
    }

    async fn append_skill_feedback(
        &self,
        feedback: SkillFeedbackEvent,
    ) -> Result<SkillFeedbackEvent, StorageError> {
        let _guard = self.skill_runtime_gate.lock().await;
        self.commit_lance_write(self.lance.append_skill_feedback(feedback).await)
            .await
    }

    async fn list_skill_feedback(
        &self,
        tenant: &str,
        skill_id: &str,
        bundle_version_id: &str,
        limit: usize,
    ) -> Result<Vec<SkillFeedbackEvent>, StorageError> {
        self.lance
            .list_skill_feedback(tenant, skill_id, bundle_version_id, limit)
            .await
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use crate::domain::{
        capability_capsule::Visibility, ResourceEntry, SkillId, SkillManifest, SKILL_DOCUMENT_PATH,
        SKILL_MANIFEST_SCHEMA_VERSION,
    };

    use super::*;

    mod tenant_isolation;

    const NOW: &str = "00000001787000000000";

    fn digest(content: &[u8]) -> String {
        format!("{:x}", Sha256::digest(content))
    }

    fn proposal(tenant: &str, proposal_id: &str, skill_id: &str) -> SkillProposalRecord {
        SkillProposalRecord {
            proposal_id: proposal_id.to_string(),
            tenant: tenant.to_string(),
            job_id: format!("job-{proposal_id}"),
            capsule_id: format!("capsule-{proposal_id}"),
            draft_json: "{\"title\":\"Example\"}".to_string(),
            provenance_json: "{\"compiler_version\":\"v1\"}".to_string(),
            target_skill_id: Some(skill_id.to_string()),
            expected_head_version: None,
            status: SkillProposalStatus::PendingConfirmation,
            created_at: NOW.to_string(),
            updated_at: NOW.to_string(),
        }
    }

    fn blob(tenant: &str, content: &[u8]) -> SkillResourceBlob {
        SkillResourceBlob {
            tenant: tenant.to_string(),
            sha256: digest(content),
            media_type: "text/markdown".to_string(),
            content: content.to_vec(),
            size_bytes: content.len() as u64,
            created_at: NOW.to_string(),
        }
    }

    async fn accepted_bundle(
        store: &Store,
        tenant: &str,
        proposal_id: &str,
        skill_id: &str,
        version: &str,
        content: &[u8],
    ) -> SkillBundleVersionRecord {
        let inserted = store
            .settle_skill_proposal(proposal(tenant, proposal_id, skill_id))
            .await
            .unwrap();
        store
            .update_skill_proposal_outcome(
                tenant,
                &inserted.proposal_id,
                SkillProposalStatus::PendingConfirmation,
                SkillProposalStatus::Accepted,
                NOW,
            )
            .await
            .unwrap();
        let blob = store
            .put_skill_resource_blob(blob(tenant, content))
            .await
            .unwrap();
        let manifest = SkillManifest {
            schema_version: SKILL_MANIFEST_SCHEMA_VERSION,
            skill_id: SkillId(skill_id.to_string()),
            resources: vec![ResourceEntry {
                path: SKILL_DOCUMENT_PATH.to_string(),
                media_type: blob.media_type.clone(),
                sha256: blob.sha256.clone(),
                size_bytes: blob.size_bytes,
                executable: false,
            }],
        };
        SkillBundleVersionRecord {
            tenant: tenant.to_string(),
            skill_id: skill_id.to_string(),
            bundle_version_id: version.to_string(),
            proposal_id: proposal_id.to_string(),
            workflow_capsule_id: format!("capsule-{proposal_id}"),
            previous_bundle_version_id: None,
            manifest_sha256: manifest.digest().unwrap(),
            manifest,
            created_at: NOW.to_string(),
        }
    }

    #[tokio::test]
    async fn bundle_and_head_writes_are_idempotent() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("runtime.lance")).await.unwrap();
        let bundle = accepted_bundle(
            &store,
            "tenant-a",
            "proposal-a",
            "skill-a",
            "v1",
            b"Example instructions",
        )
        .await;

        assert_eq!(
            store
                .append_skill_bundle_version(bundle.clone())
                .await
                .unwrap(),
            bundle
        );
        assert_eq!(
            store
                .append_skill_bundle_version(bundle.clone())
                .await
                .unwrap(),
            bundle
        );

        let head = SkillHead {
            tenant: "tenant-a".to_string(),
            skill_id: "skill-a".to_string(),
            bundle_version_id: "v1".to_string(),
            updated_at: NOW.to_string(),
        };
        assert_eq!(
            store
                .compare_and_set_skill_head(None, head.clone())
                .await
                .unwrap(),
            head
        );
        assert_eq!(
            store
                .compare_and_set_skill_head(None, head.clone())
                .await
                .unwrap(),
            head
        );
    }

    #[tokio::test]
    async fn first_seen_session_pin_stays_on_the_original_bundle() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("runtime.lance")).await.unwrap();
        for (proposal_id, version) in [("proposal-v1", "v1"), ("proposal-v2", "v2")] {
            let bundle = accepted_bundle(
                &store,
                "tenant-a",
                proposal_id,
                "skill-a",
                version,
                b"Example instructions",
            )
            .await;
            store.append_skill_bundle_version(bundle).await.unwrap();
        }
        let first = SessionSkillPin {
            tenant: "tenant-a".to_string(),
            session_id: "session-a".to_string(),
            agent_id: "agent-a".to_string(),
            skill_id: "skill-a".to_string(),
            bundle_version_id: "v1".to_string(),
            pinned_at: NOW.to_string(),
            expires_at: "00000001787000001000".to_string(),
            revision: 1,
        };
        assert_eq!(
            store.get_or_pin_session_skill(first.clone()).await.unwrap(),
            first
        );

        let requested_newer = SessionSkillPin {
            bundle_version_id: "v2".to_string(),
            ..first.clone()
        };
        assert_eq!(
            store
                .get_or_pin_session_skill(requested_newer)
                .await
                .unwrap(),
            first
        );
    }

    #[tokio::test]
    async fn expired_session_pin_repins_with_a_fenced_revision() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("runtime-expiry.lance"))
            .await
            .unwrap();
        for (proposal_id, version) in [("proposal-v1", "v1"), ("proposal-v2", "v2")] {
            let bundle = accepted_bundle(
                &store,
                "tenant-a",
                proposal_id,
                "skill-a",
                version,
                b"Example instructions",
            )
            .await;
            store.append_skill_bundle_version(bundle).await.unwrap();
        }
        let first = SessionSkillPin {
            tenant: "tenant-a".to_string(),
            session_id: "session-a".to_string(),
            agent_id: "agent-a".to_string(),
            skill_id: "skill-a".to_string(),
            bundle_version_id: "v1".to_string(),
            pinned_at: NOW.to_string(),
            expires_at: "00000001787000001000".to_string(),
            revision: 1,
        };
        store.get_or_pin_session_skill(first).await.unwrap();
        let repinned = store
            .get_or_pin_session_skill(SessionSkillPin {
                tenant: "tenant-a".to_string(),
                session_id: "session-a".to_string(),
                agent_id: "agent-a".to_string(),
                skill_id: "skill-a".to_string(),
                bundle_version_id: "v2".to_string(),
                pinned_at: "00000001787000002000".to_string(),
                expires_at: "00000001787000003000".to_string(),
                revision: 1,
            })
            .await
            .unwrap();
        assert_eq!(repinned.bundle_version_id, "v2");
        assert_eq!(repinned.revision, 2);
    }

    #[tokio::test]
    async fn feedback_append_is_idempotent() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("runtime.lance")).await.unwrap();
        let bundle = accepted_bundle(
            &store,
            "tenant-a",
            "proposal-a",
            "skill-a",
            "v1",
            b"Example instructions",
        )
        .await;
        store.append_skill_bundle_version(bundle).await.unwrap();
        let feedback = SkillFeedbackEvent {
            tenant: "tenant-a".to_string(),
            feedback_id: "feedback-a".to_string(),
            skill_id: "skill-a".to_string(),
            bundle_version_id: "v1".to_string(),
            feedback_kind: "useful".to_string(),
            note: Some("Worked as expected".to_string()),
            created_at: NOW.to_string(),
        };

        assert_eq!(
            store.append_skill_feedback(feedback.clone()).await.unwrap(),
            feedback
        );
        assert_eq!(
            store.append_skill_feedback(feedback.clone()).await.unwrap(),
            feedback
        );
        assert_eq!(
            store
                .list_skill_feedback("tenant-a", "skill-a", "v1", 10)
                .await
                .unwrap(),
            vec![feedback]
        );
    }

    #[tokio::test]
    async fn compiler_job_settlement_allows_exactly_one_terminal_receipt_kind() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path().join("terminal-receipts.lance"))
            .await
            .unwrap();

        let proposal_first = proposal("tenant-a", "proposal-first", "skill-a");
        store
            .settle_skill_proposal(proposal_first.clone())
            .await
            .unwrap();
        let conflicting_decision = SkillCompileDecisionRecord {
            job_id: proposal_first.job_id.clone(),
            tenant: "tenant-a".to_string(),
            input_fingerprint: "input-a".to_string(),
            decision_kind: "nothing_to_save".to_string(),
            canonical_signature: None,
            target_capability_capsule_id: None,
            artifact_class: None,
            reason: Some("nothing durable".to_string()),
            model_id: "test-model".to_string(),
            finish_reason: "stop".to_string(),
            prompt_tokens: 1,
            completion_tokens: 1,
            created_at: NOW.to_string(),
        };
        assert!(store
            .settle_skill_compile_decision(conflicting_decision)
            .await
            .is_err());

        let decision_first = SkillCompileDecisionRecord {
            job_id: "job-decision-first".to_string(),
            tenant: "tenant-a".to_string(),
            input_fingerprint: "input-b".to_string(),
            decision_kind: "nothing_to_save".to_string(),
            canonical_signature: None,
            target_capability_capsule_id: None,
            artifact_class: None,
            reason: Some("nothing durable".to_string()),
            model_id: "test-model".to_string(),
            finish_reason: "stop".to_string(),
            prompt_tokens: 1,
            completion_tokens: 1,
            created_at: NOW.to_string(),
        };
        store
            .settle_skill_compile_decision(decision_first)
            .await
            .unwrap();
        assert!(store
            .settle_skill_proposal(proposal("tenant-a", "decision-first", "skill-a"))
            .await
            .is_err());
    }

    #[test]
    fn loadout_binding_fixture_uses_supported_visibility() {
        let binding = AgentLoadoutBinding {
            tenant: "tenant-a".to_string(),
            agent_id: "agent-a".to_string(),
            skill_id: "skill-a".to_string(),
            mode: crate::domain::AgentLoadoutMode::FollowHead,
            priority: 10,
            enabled: true,
            visibility: Visibility::Shared,
            updated_at: NOW.to_string(),
        };
        assert_eq!(binding.visibility, Visibility::Shared);
    }
}

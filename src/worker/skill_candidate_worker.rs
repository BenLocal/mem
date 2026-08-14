use std::{sync::Arc, time::Duration};

use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

use crate::config::SkillCandidateSettings;
use crate::service::SkillCandidateService;

/// Periodically repairs the deterministic durable candidate queue. This phase
/// deliberately has no consumer: jobs remain `pending` until the separately
/// review-gated extraction phase is implemented.
pub async fn run(service: Arc<SkillCandidateService>, settings: SkillCandidateSettings) {
    let mut ticker = tokio::time::interval(Duration::from_secs(settings.interval_secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        match service.reconcile().await {
            Ok(report) => {
                crate::metrics::metrics().record_skill_candidate_reconcile(
                    report.evidence_count as u64,
                    report.inserted_job_count as u64,
                    report.existing_job_count as u64,
                    report.staled_job_count as u64,
                );
                if report.inserted_job_count > 0 {
                    info!(
                        evidence = report.evidence_count,
                        planned = report.planned_job_count,
                        inserted = report.inserted_job_count,
                        existing = report.existing_job_count,
                        staled = report.staled_job_count,
                        "skill candidate jobs reconciled"
                    );
                }
            }
            Err(error) => {
                crate::metrics::metrics().inc_skill_candidate_reconcile_error();
                warn!(error = %error, "skill candidate reconcile degraded; will retry");
            }
        }
    }
}

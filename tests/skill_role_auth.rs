use std::{ffi::OsString, sync::Arc};

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use mem::{config::Config, http, service::CapabilityCapsuleService, storage::Store};
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::util::ServiceExt;

mod common;

const COMPILER_TOKEN: &str = "compiler-role-token-integration-test-0001";
const REVIEWER_TOKEN: &str = "reviewer-role-token-integration-test-0002";
const RUNTIME_TOKEN: &str = "runtime-role-token-integration-test-0003";

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: Option<&str>) -> Self {
        let previous = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

struct TestApp {
    _temp_dir: TempDir,
    router: axum::Router,
    _store: Arc<Store>,
}

async fn test_app() -> TestApp {
    let config = {
        let _admin = EnvGuard::set("MEM_ADMIN_TOKEN", None);
        let _compiler = EnvGuard::set("MEM_SKILL_COMPILER_TOKEN", Some(COMPILER_TOKEN));
        let _reviewer = EnvGuard::set("MEM_SKILL_REVIEWER_TOKEN", Some(REVIEWER_TOKEN));
        let _runtime = EnvGuard::set("MEM_SKILL_RUNTIME_TOKEN", Some(RUNTIME_TOKEN));
        Config::from_env().expect("role-token config")
    };
    let (temp_dir, store) = common::test_store().await;
    let capsule_service = CapabilityCapsuleService::new(store.clone());
    let mut state = common::test_app_state(store.clone(), capsule_service);
    state.config = config;
    TestApp {
        _temp_dir: temp_dir,
        router: http::router().with_state(state),
        _store: store,
    }
}

async fn request(
    app: &TestApp,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> StatusCode {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    app.router
        .clone()
        .oneshot(request.body(body).expect("request build"))
        .await
        .expect("request runs")
        .status()
}

#[tokio::test]
async fn skill_role_tokens_are_mutually_scoped_to_their_routes() {
    let app = test_app().await;
    let accept_body = json!({"tenant": "local", "proposal_id": "missing-proposal"});

    assert_ne!(
        request(
            &app,
            "POST",
            "/admin/skill-proposals/claim",
            COMPILER_TOKEN,
            Some(json!({"limit": 1})),
        )
        .await,
        StatusCode::UNAUTHORIZED,
    );
    assert_ne!(
        request(
            &app,
            "GET",
            "/reviews/pending?tenant=local",
            REVIEWER_TOKEN,
            None,
        )
        .await,
        StatusCode::UNAUTHORIZED,
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/admin/skill-proposals/accept",
            COMPILER_TOKEN,
            Some(accept_body.clone()),
        )
        .await,
        StatusCode::UNAUTHORIZED,
    );

    assert_ne!(
        request(
            &app,
            "POST",
            "/admin/skill-proposals/accept",
            REVIEWER_TOKEN,
            Some(accept_body.clone()),
        )
        .await,
        StatusCode::UNAUTHORIZED,
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/admin/skill-proposals/claim",
            REVIEWER_TOKEN,
            Some(json!({"limit": 1})),
        )
        .await,
        StatusCode::UNAUTHORIZED,
    );

    for (path, body) in [
        ("/admin/skill-proposals/claim", json!({"limit": 1})),
        ("/admin/skill-proposals/accept", accept_body),
        (
            "/admin/agent-loadouts/bind",
            json!({
                "tenant": "local",
                "agent_id": "agent-a",
                "skill_id": "skill-a",
                "visibility": "shared",
            }),
        ),
    ] {
        assert_eq!(
            request(&app, "POST", path, RUNTIME_TOKEN, Some(body)).await,
            StatusCode::UNAUTHORIZED,
            "runtime token unexpectedly authorized for {path}",
        );
    }

    assert_ne!(
        request(
            &app,
            "POST",
            "/admin/agent-loadouts/resolve",
            RUNTIME_TOKEN,
            Some(json!({
                "tenant": "local",
                "agent_id": "agent-a",
                "session_id": "session-a",
            })),
        )
        .await,
        StatusCode::UNAUTHORIZED,
    );
    assert_ne!(
        request(
            &app,
            "GET",
            "/admin/skills/skill-a/versions/v1/resources/0000000000000000000000000000000000000000000000000000000000000000?tenant=local&agent_id=agent-a&session_id=session-a",
            RUNTIME_TOKEN,
            None,
        )
        .await,
        StatusCode::UNAUTHORIZED,
    );
    assert_ne!(
        request(
            &app,
            "POST",
            "/admin/skills/feedback",
            RUNTIME_TOKEN,
            Some(json!({
                "tenant": "local",
                "feedback_id": "feedback-a",
                "skill_id": "skill-a",
                "bundle_version_id": "v1",
                "feedback_kind": "outdated",
            })),
        )
        .await,
        StatusCode::UNAUTHORIZED,
    );

    for (token, path, body) in [
        (
            COMPILER_TOKEN,
            "/admin/skill-proposals/claim",
            json!({"limit": 1, "tenant": "other-tenant"}),
        ),
        (
            REVIEWER_TOKEN,
            "/admin/agent-loadouts/bind",
            json!({
                "tenant": "other-tenant",
                "agent_id": "agent-a",
                "skill_id": "skill-a",
                "visibility": "shared",
            }),
        ),
        (
            RUNTIME_TOKEN,
            "/admin/agent-loadouts/resolve",
            json!({
                "tenant": "other-tenant",
                "agent_id": "agent-a",
                "session_id": "session-a",
            }),
        ),
    ] {
        assert_eq!(
            request(&app, "POST", path, token, Some(body)).await,
            StatusCode::UNAUTHORIZED,
            "role token crossed its configured tenant on {path}",
        );
    }
}

//! Real fixture loader. Reads gitignored JSON file at env-var path.
//! Returns `Ok(None)` if env var is unset (callers skip silently).

use super::fixture::*;
use serde::Deserialize;
use std::path::Path;

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
struct RealFixtureFile {
    loader_version: u32,
    tenant: String,
    sessions: Vec<RealSession>,
    queries: Vec<RealQuery>,
}

#[derive(Debug, Deserialize)]
struct RealSession {
    session_id: String,
    started_at: String,
    blocks: Vec<RealBlock>,
}

#[derive(Debug, Deserialize)]
struct RealBlock {
    block_id: String,
    role: String,
    block_type: String,
    content: String,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct RealQuery {
    query_id: String,
    text: String,
    #[serde(default)]
    anchor_session_id: Option<String>,
    #[serde(default)]
    anchor_entities: Vec<String>,
}

/// Load the real fixture if `MEM_BENCH_FIXTURE_PATH` is set.
/// Returns `Ok(None)` when env var is unset.
/// Panics with a clear message if the file is missing or invalid.
pub fn load_from_env() -> std::io::Result<Option<Fixture>> {
    let path = match std::env::var("MEM_BENCH_FIXTURE_PATH") {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    Ok(Some(load_from_path(Path::new(&path))?))
}

pub fn load_from_path(path: &Path) -> std::io::Result<Fixture> {
    let bytes = std::fs::read(path)?;
    let raw: RealFixtureFile =
        serde_json::from_slice(&bytes).expect("invalid JSON in real fixture");
    if raw.loader_version != SCHEMA_VERSION {
        panic!(
            "real fixture loader_version mismatch: file says {}, code expects {}. \
             Re-export the fixture or upgrade the loader.",
            raw.loader_version, SCHEMA_VERSION
        );
    }
    let sessions: Vec<SessionFixture> = raw
        .sessions
        .into_iter()
        .map(|rs| SessionFixture {
            session_id: rs.session_id,
            started_at: rs.started_at,
            blocks: rs
                .blocks
                .into_iter()
                .map(|rb| BlockFixture {
                    block_id: rb.block_id,
                    role: rb.role,
                    block_type: rb.block_type,
                    content: rb.content,
                    created_at: rb.created_at,
                })
                .collect(),
        })
        .collect();
    let queries: Vec<QueryFixture> = raw
        .queries
        .into_iter()
        .map(|rq| QueryFixture {
            query_id: rq.query_id,
            text: rq.text,
            anchor_session_id: rq.anchor_session_id,
            anchor_entities: rq.anchor_entities,
            synthetic_judgments: None, // judgment.rs derives via entity registry
        })
        .collect();
    Ok(Fixture {
        kind: FixtureKind::Real,
        tenant: raw.tenant,
        sessions,
        queries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_fixture(json: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn load_from_env_returns_none_when_unset() {
        // Save then unset env var to ensure clean state.
        let original = std::env::var("MEM_BENCH_FIXTURE_PATH").ok();
        std::env::remove_var("MEM_BENCH_FIXTURE_PATH");
        let res = load_from_env().unwrap();
        assert!(res.is_none());
        if let Some(v) = original {
            std::env::set_var("MEM_BENCH_FIXTURE_PATH", v);
        }
    }

    #[test]
    fn load_from_path_parses_minimal_valid_file() {
        let json = r#"{
            "loader_version": 1,
            "tenant": "local",
            "sessions": [{
                "session_id": "s1",
                "started_at": "00000000020260503000",
                "blocks": [{
                    "block_id": "b1",
                    "role": "user",
                    "block_type": "text",
                    "content": "hello",
                    "created_at": "00000000020260503000"
                }]
            }],
            "queries": [{
                "query_id": "q1",
                "text": "hi",
                "anchor_entities": ["greeting"]
            }]
        }"#;
        let f = write_fixture(json);
        let fixture = load_from_path(f.path()).unwrap();
        assert_eq!(fixture.tenant, "local");
        assert_eq!(fixture.sessions.len(), 1);
        assert_eq!(fixture.queries.len(), 1);
        assert_eq!(fixture.queries[0].anchor_entities, vec!["greeting"]);
        assert!(fixture.queries[0].synthetic_judgments.is_none());
        assert_eq!(fixture.kind, FixtureKind::Real);
    }

    #[test]
    #[should_panic(expected = "loader_version mismatch")]
    fn wrong_version_panics() {
        let json = r#"{
            "loader_version": 99,
            "tenant": "local",
            "sessions": [],
            "queries": []
        }"#;
        let f = write_fixture(json);
        let _ = load_from_path(f.path()).unwrap();
    }

    #[test]
    fn missing_anchor_session_id_defaults_to_none() {
        let json = r#"{
            "loader_version": 1,
            "tenant": "local",
            "sessions": [],
            "queries": [{
                "query_id": "q1",
                "text": "hi",
                "anchor_entities": []
            }]
        }"#;
        let f = write_fixture(json);
        let fixture = load_from_path(f.path()).unwrap();
        assert_eq!(fixture.queries[0].anchor_session_id, None);
    }
}

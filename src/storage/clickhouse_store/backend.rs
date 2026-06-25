//! [`ClickHouseBackend`] — the `clickhouse::Client` wrapper.
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P1).**

use clickhouse::Client;

use crate::storage::types::StorageError;

/// ClickHouse backend handle. Holds a clickhouse-rs [`Client`] (cheap to
/// clone; pools connections internally). Implements [`CapsuleStore`] today
/// (P1); the remaining sub-traits + `Backend` arrive in P2+.
///
/// [`CapsuleStore`]: crate::storage::capsule_store::CapsuleStore
pub struct ClickHouseBackend {
    pub(crate) client: Client,
}

impl ClickHouseBackend {
    /// Build a backend from a `MEM_CLICKHOUSE_URL` (e.g.
    /// `http://localhost:8123`). The clickhouse-rs client is lazy — it
    /// opens no socket here, so a bad URL surfaces on first query, not at
    /// construction. Kept `async` to mirror `PostgresCapsuleStore::connect`
    /// and leave room for an eager ping in P2.
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(StorageError::InvalidInput(
                "MEM_CLICKHOUSE_URL is empty".to_owned(),
            ));
        }
        // Pull optional `user:password@` userinfo out of the URL (real
        // ClickHouse requires auth; the `clickhouse` crate takes the
        // endpoint via `with_url` and creds via `with_user`/`with_password`
        // separately, so split them here — same shape as the Postgres
        // backend's `MEM_POSTGRES_URL`).
        let (endpoint, user, password) = split_userinfo(url);
        let mut client = Client::default().with_url(endpoint);
        if let Some(u) = user {
            client = client.with_user(u);
        }
        if let Some(p) = password {
            client = client.with_password(p);
        }
        Ok(Self { client })
    }

    /// Idempotently apply `migrations/clickhouse/0001_capsule_store.sql`
    /// (`CREATE TABLE IF NOT EXISTS …`). clickhouse-rs runs one statement
    /// per `execute()`, so the file is split on `;`. Used by the gated
    /// integration test; not on the serve path.
    pub async fn apply_migrations(&self) -> Result<(), StorageError> {
        // Ordered list — embedded at compile time so the binary is
        // self-contained (no migrations dir needed at runtime).
        const MIGRATIONS: &[&str] = &[
            include_str!("../../../migrations/clickhouse/0001_capsule_store.sql"),
            include_str!("../../../migrations/clickhouse/0002_embeddings.sql"),
            include_str!("../../../migrations/clickhouse/0003_graph_transcript_jobs.sql"),
            include_str!("../../../migrations/clickhouse/0004_registry_session_misc.sql"),
        ];
        for sql in MIGRATIONS {
            // Strip `--` line comments from the WHOLE file FIRST, then split on
            // ';'. Order matters: a comment line may itself contain a ';' (e.g.
            // "the trait's `&str` surface; lexicographic compare ..."), so
            // splitting first would chop the comment mid-line and leak its tail
            // (no longer `--`-prefixed) onto the next statement — a stray
            // "...)" then trips CH's parser with "Unmatched parentheses".
            let no_comments: String = sql
                .lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n");
            for stmt in no_comments.split(';') {
                let trimmed = stmt.trim();
                if trimmed.is_empty() {
                    continue;
                }
                self.client.query(trimmed).execute().await.map_err(|e| {
                    StorageError::InvalidInput(format!("clickhouse migration: {e}"))
                })?;
            }
        }
        Ok(())
    }
}

/// Split `scheme://[user[:password]@]host…` into (endpoint-without-userinfo,
/// user, password). Returns the URL unchanged with `None`s when there is no
/// `@` userinfo. The `clickhouse` crate wants the endpoint and creds
/// separately, so we peel the userinfo off `MEM_CLICKHOUSE_URL` here.
fn split_userinfo(url: &str) -> (String, Option<String>, Option<String>) {
    let Some(scheme_end) = url.find("://") else {
        return (url.to_owned(), None, None);
    };
    let rest = &url[scheme_end + 3..];
    let Some(at) = rest.find('@') else {
        return (url.to_owned(), None, None);
    };
    let userinfo = &rest[..at];
    let host = &rest[at + 1..];
    let (user, password) = match userinfo.split_once(':') {
        Some((u, p)) => (u, Some(p.to_owned())),
        None => (userinfo, None),
    };
    let user = if user.is_empty() {
        None
    } else {
        Some(user.to_owned())
    };
    (format!("{}{host}", &url[..scheme_end + 3]), user, password)
}

#[cfg(test)]
mod tests {
    use super::split_userinfo;

    #[test]
    fn split_userinfo_handles_creds_and_bare_url() {
        let (e, u, p) = split_userinfo("http://localhost:8123");
        assert_eq!(e, "http://localhost:8123");
        assert!(u.is_none() && p.is_none());

        let (e, u, p) = split_userinfo("http://mem:secret@ch.example:8123");
        assert_eq!(e, "http://ch.example:8123");
        assert_eq!(u.as_deref(), Some("mem"));
        assert_eq!(p.as_deref(), Some("secret"));

        let (e, u, p) = split_userinfo("http://default@localhost:8123");
        assert_eq!(e, "http://localhost:8123");
        assert_eq!(u.as_deref(), Some("default"));
        assert!(p.is_none());
    }
}

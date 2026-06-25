//! `mem sync` — verbatim store-to-store copy (any → any across Lance /
//! Postgres / ClickHouse). See docs/superpowers/specs/2026-06-25-store-sync-cli-design.md.

use crate::config::BackendKind;

/// Parse a `--from` / `--to` spec of the form `<kind>:<locator>` into a
/// `(BackendKind, locator)` pair. `kind` is `lance` | `postgres` |
/// `clickhouse`; `locator` is the remainder after the FIRST `:` (so URLs
/// keeping their own `://` survive intact). Errors on unknown kind or
/// empty locator.
#[allow(dead_code)] // wired up in Task 2
pub fn parse_spec(spec: &str) -> anyhow::Result<(BackendKind, String)> {
    let (kind_str, locator) = spec
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("spec must be `<kind>:<locator>`, got `{spec}`"))?;
    let kind = match kind_str {
        "lance" => BackendKind::Lance,
        "postgres" => BackendKind::Postgres,
        "clickhouse" => BackendKind::Clickhouse,
        other => anyhow::bail!("unknown backend kind `{other}` (use lance|postgres|clickhouse)"),
    };
    if locator.is_empty() {
        anyhow::bail!("spec `{spec}` has an empty locator");
    }
    Ok((kind, locator.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lance_dir() {
        let (k, loc) = parse_spec("lance:/root/.mem/mem.lance").unwrap();
        assert_eq!(k, BackendKind::Lance);
        assert_eq!(loc, "/root/.mem/mem.lance");
    }

    #[test]
    fn parses_postgres_url_keeping_scheme() {
        let (k, loc) = parse_spec("postgres:postgres://u:p@h:5432/db").unwrap();
        assert_eq!(k, BackendKind::Postgres);
        assert_eq!(loc, "postgres://u:p@h:5432/db");
    }

    #[test]
    fn parses_clickhouse_url() {
        let (k, loc) = parse_spec("clickhouse:http://mem:mem@localhost:8123").unwrap();
        assert_eq!(k, BackendKind::Clickhouse);
        assert_eq!(loc, "http://mem:mem@localhost:8123");
    }

    #[test]
    fn rejects_unknown_kind() {
        assert!(parse_spec("mysql:whatever").is_err());
    }

    #[test]
    fn rejects_missing_locator() {
        assert!(parse_spec("lance:").is_err());
        assert!(parse_spec("lance").is_err());
    }
}

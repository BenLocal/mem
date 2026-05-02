use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entity {
    pub entity_id: String,
    pub tenant: String,
    pub canonical_name: String,
    pub kind: EntityKind,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum EntityKind {
    Topic,
    Project,
    Repo,
    Module,
    Workflow,
}

impl EntityKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            EntityKind::Topic => "topic",
            EntityKind::Project => "project",
            EntityKind::Repo => "repo",
            EntityKind::Module => "module",
            EntityKind::Workflow => "workflow",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "topic" => Some(EntityKind::Topic),
            "project" => Some(EntityKind::Project),
            "repo" => Some(EntityKind::Repo),
            "module" => Some(EntityKind::Module),
            "workflow" => Some(EntityKind::Workflow),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntityWithAliases {
    pub entity: Entity,
    /// Normalized alias forms, ordered by created_at ASC. The first-written
    /// alias (added by `resolve_or_create` itself) is at index 0.
    pub aliases: Vec<String>,
}

/// Result of `EntityRegistry::add_alias`. HTTP layer maps these to status
/// codes: Inserted/AlreadyOnSameEntity → 200, ConflictWithDifferentEntity → 409.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddAliasOutcome {
    Inserted,
    AlreadyOnSameEntity,
    ConflictWithDifferentEntity(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_kind_round_trip_db_str() {
        for k in [
            EntityKind::Topic,
            EntityKind::Project,
            EntityKind::Repo,
            EntityKind::Module,
            EntityKind::Workflow,
        ] {
            assert_eq!(EntityKind::from_db_str(k.as_db_str()), Some(k));
        }
    }

    #[test]
    fn entity_kind_from_db_str_rejects_unknown() {
        assert_eq!(EntityKind::from_db_str(""), None);
        assert_eq!(EntityKind::from_db_str("Topic"), None); // case-sensitive
        assert_eq!(EntityKind::from_db_str("project "), None); // no trim
        assert_eq!(EntityKind::from_db_str("bogus"), None);
    }

    #[test]
    fn entity_kind_serializes_lowercase() {
        let k = EntityKind::Project;
        let s = serde_json::to_string(&k).unwrap();
        assert_eq!(s, "\"project\"");
    }

    #[test]
    fn add_alias_outcome_matches() {
        let inserted = AddAliasOutcome::Inserted;
        assert_eq!(inserted, AddAliasOutcome::Inserted);
        let conflict = AddAliasOutcome::ConflictWithDifferentEntity("e1".into());
        match conflict {
            AddAliasOutcome::ConflictWithDifferentEntity(id) => assert_eq!(id, "e1"),
            _ => panic!("variant mismatch"),
        }
    }
}

//! Full-text index over tool names and descriptions: SQLite FTS5 in memory, through SeaORM.
//! It holds nothing durable, so it stays out of `fronde.db`, and is rebuilt from the registry
//! at the first search after the tools changed.

use crate::tools::ToolDef;
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement,
    TransactionTrait,
};
use tokio::sync::Mutex;

/// Turn free text into a safe FTS5 query: every word quoted, prefix-matched, OR-joined.
pub fn fts_query(text: &str) -> Option<String> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect();
    (!words.is_empty()).then(|| words.join(" OR "))
}

#[derive(Default)]
pub struct ToolIndex {
    /// Created at the first search; `None` again after an error, so the next search starts over.
    built: Mutex<Option<Built>>,
}

struct Built {
    db: DatabaseConnection,
    /// The registry generation the table holds.
    generation: u64,
}

/// One in-memory database. Each SQLite connection to `:memory:` has its own database, so the
/// pool keeps exactly one connection and never closes it for being idle or old.
async fn connect() -> Result<DatabaseConnection, sea_orm::DbErr> {
    let mut options = ConnectOptions::new("sqlite::memory:");
    options
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .sqlx_logging(false);
    Database::connect(options).await
}

async fn rebuild(db: &DatabaseConnection, tools: &[ToolDef]) -> Result<(), sea_orm::DbErr> {
    let tx = db.begin().await?;
    tx.execute_unprepared(
        "CREATE VIRTUAL TABLE IF NOT EXISTS t USING fts5(name, description, source, \
         tokenize='unicode61 remove_diacritics 2')",
    )
    .await?;
    tx.execute_unprepared("DELETE FROM t").await?;
    for def in tools {
        tx.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO t(name, description, source) VALUES (?, ?, ?)",
            [
                def.name.as_str().into(),
                def.description.as_str().into(),
                def.source.as_str().into(),
            ],
        ))
        .await?;
    }
    tx.commit().await
}

async fn query(
    db: &DatabaseConnection,
    q: String,
    limit: usize,
) -> Result<Vec<String>, sea_orm::DbErr> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT name FROM t WHERE t MATCH ? ORDER BY bm25(t) LIMIT ?",
            [q.into(), (limit as i64).into()],
        ))
        .await?;
    rows.iter().map(|r| r.try_get_by_index(0)).collect()
}

impl ToolIndex {
    /// Names of the tools matching `query_text`, best first. `generation` names the registry's
    /// current tools: when the index holds another, `tools` lists them and the index is rebuilt.
    pub async fn search(
        &self,
        generation: u64,
        tools: impl FnOnce() -> Vec<ToolDef>,
        query_text: &str,
        limit: usize,
    ) -> Vec<String> {
        let Some(q) = fts_query(query_text) else {
            return Vec::new();
        };
        let mut built = self.built.lock().await;
        let result = async {
            if built.as_ref().is_none_or(|b| b.generation != generation) {
                let db = match built.take() {
                    Some(b) => b.db,
                    None => connect().await?,
                };
                rebuild(&db, &tools()).await?;
                *built = Some(Built { db, generation });
            }
            let db = &built.as_ref().expect("built above").db;
            query(db, q, limit).await
        }
        .await;
        result.unwrap_or_else(|e| {
            tracing::warn!("tool search: {e}");
            *built = None;
            Vec::new()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Registry, ToolDef, ToolOutput, meta_tools, typed_tool};
    use serde_json::json;
    use std::sync::Arc;

    fn def(name: &str, description: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: description.into(),
            input_schema: json!({"type": "object"}),
            deferred: true,
            source: "mcphub".into(),
        }
    }

    #[tokio::test]
    async fn finds_by_description_words() {
        let i = ToolIndex::default();
        let ha = def("mcphub_ha_turn_on", "Turn on a light or switch");
        let mealie = def("mcphub_mealie_add_item", "Add an item to the shopping list");
        let both = vec![ha, mealie.clone()];
        assert_eq!(
            i.search(1, || both, "shopping list", 5).await[0],
            "mcphub_mealie_add_item"
        );
        // Same generation: the index is not rebuilt.
        assert_eq!(
            i.search(1, || unreachable!(), "turn on", 5).await[0],
            "mcphub_ha_turn_on"
        );
        // The light tool is gone in the next generation.
        assert!(i.search(2, || vec![mealie], "light", 5).await.is_empty());
    }

    #[test]
    fn fts_query_quotes_every_word() {
        assert_eq!(fts_query("a\"b").as_deref(), Some("\"a\"* OR \"b\"*"));
        assert_eq!(
            fts_query("NEAR(a b)").as_deref(),
            Some("\"NEAR\"* OR \"a\"* OR \"b\"*")
        );
        assert_eq!(fts_query("a*").as_deref(), Some("\"a\"*"));
        assert_eq!(fts_query("(a").as_deref(), Some("\"a\"*"));
        assert_eq!(fts_query("name:a").as_deref(), Some("\"name\"* OR \"a\"*"));
        assert_eq!(
            fts_query("a AND -b").as_deref(),
            Some("\"a\"* OR \"AND\"* OR \"b\"*")
        );
        for q in ["", "*", "(", ":", "\"", "-:*(\""] {
            assert_eq!(fts_query(q), None, "{q:?} has no word");
        }
    }

    #[tokio::test]
    async fn search_survives_fts_syntax() {
        let i = ToolIndex::default();
        let tools = || vec![def("a_b", "Something")];
        // A query error gives no hits, so each hostile query also carries a real word: finding
        // the tool proves the query reached FTS5 as valid syntax.
        for q in [
            "\"-:*( AND",
            "",
            "NEAR(",
            "a\"b",
            "*",
            "OR OR",
            "name:",
            "(",
            "\"",
        ] {
            assert_eq!(
                i.search(1, tools, &format!("{q} something"), 5).await,
                vec!["a_b"],
                "query {q:?}"
            );
        }
    }

    #[tokio::test]
    async fn search_ignores_accents() {
        let i = ToolIndex::default();
        let tools = || vec![def("mcphub_courses", "Liste des courses")];
        assert_eq!(
            i.search(1, tools, "liste courses", 5).await,
            vec!["mcphub_courses"]
        );
        assert_eq!(i.search(1, tools, "lìste", 5).await, vec!["mcphub_courses"]);
    }

    fn registry_with_echo() -> Arc<Registry> {
        let r = Arc::new(Registry::new());
        r.replace_source("hub", meta_tools(&r, None, None));
        let (mut d, h) = typed_tool(
            "mcphub_echo",
            "Echo text back to the caller",
            true,
            |v: serde_json::Value| async move { ToolOutput::ok(v) },
        );
        d.source = "mcphub".into();
        r.register(d, h).unwrap();
        let (d, h) = typed_tool(
            "memory_like",
            "Echo text but visible",
            false,
            |v: serde_json::Value| async move { ToolOutput::ok(v) },
        );
        r.register(d, h).unwrap();
        r
    }

    #[tokio::test]
    async fn search_tools_returns_only_deferred() {
        let r = registry_with_echo();
        let out = r.call("search_tools", json!({"need": "echo text"})).await;
        assert!(!out.is_error, "{}", out.to_text());
        let names: Vec<_> = out
            .value
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["mcphub_echo"]);
        assert!(out.value[0]["input_schema"].is_object());
        assert!(out.next.unwrap().contains("use_tool"));
        let none = r.call("search_tools", json!({"need": "zzzz"})).await;
        assert_eq!(none.value, json!([]));
        assert!(none.next.unwrap().contains("Rephrase"));
    }

    #[tokio::test]
    async fn use_tool_calls_deferred() {
        let r = registry_with_echo();
        let out = r
            .call(
                "use_tool",
                json!({"name": "mcphub_echo", "arguments": {"x": 1}}),
            )
            .await;
        assert_eq!(out.value, json!({"x": 1}));
        let missing = r
            .call("use_tool", json!({"name": "nope", "arguments": {}}))
            .await;
        assert!(missing.is_error);
    }

    #[test]
    fn meta_tools_are_visible() {
        let r = registry_with_echo();
        let names: Vec<_> = r.visible().into_iter().map(|d| d.name).collect();
        assert!(
            names.contains(&"search_tools".to_string()) && names.contains(&"use_tool".to_string())
        );
    }
}

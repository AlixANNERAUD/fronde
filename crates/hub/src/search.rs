//! Full-text index over tool names and descriptions (SQLite FTS5, in memory).

use crate::tools::ToolDef;
use rusqlite::{Connection, params};
use std::sync::Mutex;

pub struct ToolIndex {
    db: Mutex<Connection>,
}

impl Default for ToolIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Turn free text into a safe FTS5 query: every word quoted, prefix-matched, OR-joined.
pub fn fts_query(text: &str) -> Option<String> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect();
    (!words.is_empty()).then(|| words.join(" OR "))
}

impl ToolIndex {
    pub fn new() -> Self {
        let db = Connection::open_in_memory().expect("in-memory sqlite");
        db.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(name, description, source, \
             tokenize='unicode61 remove_diacritics 2');",
        )
        .expect("fts5 table");
        Self { db: Mutex::new(db) }
    }

    pub fn upsert(&self, def: &ToolDef) {
        let db = self.db.lock().expect("index lock");
        let _ = db.execute("DELETE FROM t WHERE name = ?1", params![def.name]);
        let _ = db.execute(
            "INSERT INTO t(name, description, source) VALUES (?1, ?2, ?3)",
            params![def.name, def.description, def.source],
        );
    }

    pub fn remove(&self, name: &str) {
        let db = self.db.lock().expect("index lock");
        let _ = db.execute("DELETE FROM t WHERE name = ?1", params![name]);
    }

    /// Names of matching tools, best first.
    pub fn search(&self, query: &str, limit: usize) -> Vec<String> {
        let Some(q) = fts_query(query) else {
            return Vec::new();
        };
        let db = self.db.lock().expect("index lock");
        let Ok(mut stmt) =
            db.prepare("SELECT name FROM t WHERE t MATCH ?1 ORDER BY bm25(t) LIMIT ?2")
        else {
            return Vec::new();
        };
        stmt.query_map(params![q, limit as i64], |row| row.get(0))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Registry, ToolDef, ToolOutput, register_meta_tools, typed_tool};
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

    #[test]
    fn finds_by_description_words() {
        let i = ToolIndex::new();
        i.upsert(&def("mcphub_ha_turn_on", "Turn on a light or switch"));
        i.upsert(&def(
            "mcphub_mealie_add_item",
            "Add an item to the shopping list",
        ));
        assert_eq!(i.search("shopping list", 5)[0], "mcphub_mealie_add_item");
        assert_eq!(i.search("turn on", 5)[0], "mcphub_ha_turn_on");
        i.remove("mcphub_ha_turn_on");
        assert!(i.search("light", 5).is_empty());
    }

    #[test]
    fn search_survives_fts_syntax() {
        let i = ToolIndex::new();
        i.upsert(&def("a_b", "Something"));
        for q in ["\"-:*( AND", "", "NEAR(", "a\"b", "*", "OR OR"] {
            let _ = i.search(q, 5);
        }
    }

    #[test]
    fn search_ignores_accents() {
        let i = ToolIndex::new();
        i.upsert(&def("mcphub_courses", "Liste des courses"));
        assert_eq!(i.search("liste courses", 5), vec!["mcphub_courses"]);
        assert_eq!(i.search("lìste", 5), vec!["mcphub_courses"]);
    }

    fn registry_with_echo() -> Arc<Registry> {
        let r = Arc::new(Registry::new());
        register_meta_tools(&r).unwrap();
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

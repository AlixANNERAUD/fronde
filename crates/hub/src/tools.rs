//! Tool registry: every tool (native, upstream, meta) goes through here.

use crate::config::is_valid_name;
use crate::search::ToolIndex;
use crate::source::Tool;
use anyhow::bail;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub deferred: bool,
    /// Where the tool comes from: "hub" for native tools, the upstream server's name for its tools,
    /// the component's name for a component's.
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub value: Value,
    pub next: Option<String>,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(v: impl Serialize) -> Self {
        let value = serde_json::to_value(v).unwrap_or_else(|e| Value::String(e.to_string()));
        Self {
            value,
            next: None,
            is_error: false,
        }
    }

    pub fn with_next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(next.into());
        self
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            value: Value::String(msg.into()),
            next: None,
            is_error: true,
        }
    }

    /// `{"result":…,"next":…}` or `{"error":…}`.
    pub fn to_text(&self) -> String {
        let v = if self.is_error {
            json!({ "error": self.value })
        } else {
            let mut m = serde_json::Map::new();
            m.insert("result".into(), self.value.clone());
            if let Some(n) = &self.next {
                m.insert("next".into(), Value::String(n.clone()));
            }
            Value::Object(m)
        };
        v.to_string()
    }
}

#[async_trait]
pub trait ToolHandler: Send + Sync {
    async fn call(&self, args: Value) -> ToolOutput;
}

struct Typed<A, F, Fut> {
    f: F,
    _types: PhantomData<fn(A) -> Fut>,
}

#[async_trait]
impl<A, F, Fut> ToolHandler for Typed<A, F, Fut>
where
    A: DeserializeOwned + Send,
    F: Fn(A) -> Fut + Send + Sync,
    Fut: Future<Output = ToolOutput> + Send,
{
    async fn call(&self, args: Value) -> ToolOutput {
        match serde_path_to_error::deserialize::<_, A>(args) {
            Ok(a) => (self.f)(a).await,
            Err(e) => ToolOutput::error(format!(
                "invalid arguments at `{}`: {}; fix them to match the tool's input schema",
                e.path(),
                e.inner()
            )),
        }
    }
}

/// Build a tool whose arguments are deserialized into `A` and whose schema comes from `A`.
pub fn typed_tool<A, F, Fut>(
    name: &str,
    description: &str,
    deferred: bool,
    f: F,
) -> (ToolDef, Arc<dyn ToolHandler>)
where
    A: DeserializeOwned + JsonSchema + Send + 'static,
    F: Fn(A) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ToolOutput> + Send + 'static,
{
    let schema =
        serde_json::to_value(schemars::schema_for!(A)).unwrap_or(json!({"type": "object"}));
    let def = ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: schema,
        deferred,
        source: "hub".to_string(),
    };
    (
        def,
        Arc::new(Typed {
            f,
            _types: PhantomData,
        }),
    )
}

type Entry = Tool;

#[derive(Default)]
pub struct Registry {
    tools: RwLock<BTreeMap<String, Entry>>,
    index: ToolIndex,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, def: ToolDef, handler: Arc<dyn ToolHandler>) -> anyhow::Result<()> {
        let mut tools = self.tools.write().expect("registry lock");
        self.insert(&mut tools, def, handler)
    }

    fn insert(
        &self,
        tools: &mut BTreeMap<String, Entry>,
        def: ToolDef,
        handler: Arc<dyn ToolHandler>,
    ) -> anyhow::Result<()> {
        if !is_valid_name(&def.name) {
            bail!(
                "tool name {:?} must match ^[a-z][a-z0-9_]{{0,63}}$",
                def.name
            );
        }
        if let Some((other, _)) = tools.get(&def.name) {
            bail!(
                "tool {:?} is already registered by {}",
                def.name,
                other.source
            );
        }
        if def.deferred {
            self.index.upsert(&def);
        }
        tools.insert(def.name.clone(), (def, handler));
        Ok(())
    }

    fn remove_source(&self, tools: &mut BTreeMap<String, Entry>, source: &str) {
        tools.retain(|name, (d, _)| {
            let keep = d.source != source;
            if !keep {
                self.index.remove(name);
            }
            keep
        });
    }

    /// Put `tools` in place of the tools of `source`, all at once: no call sees the source with
    /// some or none of its tools. Each tool takes `source` as its source; one with an invalid
    /// name or the name of another source's tool is skipped with a warning. Returns how many
    /// were registered.
    pub fn replace_source(&self, source: &str, tools: Vec<Tool>) -> usize {
        let mut registered = self.tools.write().expect("registry lock");
        self.remove_source(&mut registered, source);
        let mut count = 0;
        for (mut def, handler) in tools {
            def.source = source.to_string();
            let name = def.name.clone();
            match self.insert(&mut registered, def, handler) {
                Ok(()) => count += 1,
                Err(e) => tracing::warn!("{source}: skipped tool {name}: {e:#}"),
            }
        }
        count
    }

    /// Deferred tools matching a need, best first.
    pub fn search(&self, query: &str, limit: usize) -> Vec<ToolDef> {
        self.index
            .search(query, limit)
            .iter()
            .filter_map(|n| self.get(n))
            .collect()
    }

    /// Every tool, deferred or not.
    pub fn all(&self) -> Vec<ToolDef> {
        let tools = self.tools.read().expect("registry lock");
        tools.values().map(|(d, _)| d.clone()).collect()
    }

    pub fn visible(&self) -> Vec<ToolDef> {
        let tools = self.tools.read().expect("registry lock");
        tools
            .values()
            .filter(|(d, _)| !d.deferred)
            .map(|(d, _)| d.clone())
            .collect()
    }

    pub fn deferred(&self) -> Vec<ToolDef> {
        let tools = self.tools.read().expect("registry lock");
        tools
            .values()
            .filter(|(d, _)| d.deferred)
            .map(|(d, _)| d.clone())
            .collect()
    }

    pub fn deferred_names(&self) -> Vec<String> {
        let tools = self.tools.read().expect("registry lock");
        tools
            .values()
            .filter(|(d, _)| d.deferred)
            .map(|(d, _)| d.name.clone())
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<ToolDef> {
        self.tools
            .read()
            .expect("registry lock")
            .get(name)
            .map(|(d, _)| d.clone())
    }

    pub async fn call(&self, name: &str, args: Value) -> ToolOutput {
        let handler = self
            .tools
            .read()
            .expect("registry lock")
            .get(name)
            .map(|(_, h)| h.clone());
        match handler {
            Some(h) => h.call(args).await,
            None => ToolOutput::error(format!(
                "unknown tool {name:?}; call search_tools with your need to find the right one"
            )),
        }
    }
}

#[derive(serde::Deserialize, JsonSchema)]
struct SearchArgs {
    /// What you need to do, in a few words.
    need: String,
    /// Maximum results (default 5, max 10).
    limit: Option<u8>,
}

#[derive(serde::Deserialize, JsonSchema)]
struct UseArgs {
    /// Tool name returned by search_tools.
    name: String,
    /// Arguments matching that tool's input schema.
    #[serde(default)]
    arguments: Value,
}

/// `search_tools` and `use_tool`, over `registry`.
pub fn meta_tools(registry: &Arc<Registry>) -> Vec<Tool> {
    let r = Arc::downgrade(registry);
    let (def, h) = typed_tool(
        "search_tools",
        "Find extra tools by need (e.g. 'add to shopping list'). Returns their schemas. Use before use_tool.",
        false,
        move |a: SearchArgs| {
            let r = r.clone();
            async move {
                let Some(r) = r.upgrade() else {
                    return ToolOutput::error("hub is shutting down");
                };
                let limit = a.limit.unwrap_or(5).clamp(1, 10) as usize;
                let found: Vec<Value> = r
                    .search(&a.need, limit)
                    .into_iter()
                    .map(|d| json!({"name": d.name, "description": d.description, "input_schema": d.input_schema}))
                    .collect();
                if found.is_empty() {
                    ToolOutput::ok(found)
                        .with_next("No tool matches. Rephrase the need, or answer without a tool.")
                } else {
                    ToolOutput::ok(found).with_next("Call use_tool with one of these names.")
                }
            }
        },
    );
    let search = (def, h);
    let r = Arc::downgrade(registry);
    let (def, h) = typed_tool(
        "use_tool",
        "Call a tool found with search_tools.",
        false,
        move |a: UseArgs| {
            let r = r.clone();
            async move {
                let Some(r) = r.upgrade() else {
                    return ToolOutput::error("hub is shutting down");
                };
                if a.name == "use_tool" {
                    return ToolOutput::error(
                        "use_tool cannot call itself; pass the name of the tool you need",
                    );
                }
                r.call(&a.name, a.arguments).await
            }
        },
    );
    vec![search, (def, h)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Deserialize, schemars::JsonSchema)]
    struct A {
        n: u32,
    }

    fn double(deferred: bool, name: &str) -> (ToolDef, Arc<dyn ToolHandler>) {
        typed_tool(name, "Double a number.", deferred, |a: A| async move {
            ToolOutput::ok(a.n * 2)
        })
    }

    #[tokio::test]
    async fn register_rejects_bad_name_and_duplicate() {
        let r = Registry::new();
        let (d, h) = double(false, "x");
        let mut bad = d.clone();
        bad.name = "Agenda.list".into();
        assert!(r.register(bad, h.clone()).is_err());
        r.register(d.clone(), h.clone()).unwrap();
        let e = r.register(d, h).unwrap_err().to_string();
        assert!(e.contains("already"), "{e}");
    }

    #[tokio::test]
    async fn typed_tool_validates_args() {
        let r = Registry::new();
        let (d, h) = double(false, "double");
        assert_eq!(d.input_schema["properties"]["n"]["type"], "integer");
        r.register(d, h).unwrap();
        let bad = r.call("double", json!({"n": "a"})).await;
        assert!(bad.is_error);
        let t = bad.to_text();
        assert!(t.contains("`n`") && t.contains("expected"), "{t}");
        let ok = r.call("double", json!({"n": 2})).await;
        assert!(!ok.is_error);
        assert_eq!(ok.value, json!(4));
    }

    #[tokio::test]
    async fn call_unknown_tool_points_to_search() {
        let out = Registry::new().call("nope", json!({})).await;
        assert!(out.is_error);
        assert!(out.to_text().contains("search_tools"));
    }

    #[test]
    fn visible_excludes_deferred() {
        let r = Registry::new();
        let (d, h) = double(false, "b_visible");
        r.register(d, h).unwrap();
        let (d, h) = double(true, "a_hidden");
        r.register(d, h).unwrap();
        let names: Vec<_> = r.visible().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["b_visible"]);
        assert_eq!(r.deferred_names(), vec!["a_hidden"]);
    }

    #[test]
    fn replacing_a_source_with_nothing_removes_only_that_source() {
        let r = Registry::new();
        let (mut d, h) = double(true, "up_one");
        d.source = "up".into();
        r.register(d, h).unwrap();
        let (d, h) = double(false, "mine");
        r.register(d, h).unwrap();
        assert_eq!(r.replace_source("up", vec![]), 0);
        assert!(r.deferred_names().is_empty());
        assert_eq!(r.visible().len(), 1);
    }

    #[test]
    fn replace_source_swaps_its_tools_and_skips_clashes() {
        let r = Registry::new();
        let (d, h) = double(false, "taken");
        r.register(d, h).unwrap();
        let count = r.replace_source(
            "up",
            vec![
                double(true, "up_a"),
                double(true, "up_b"),
                double(false, "taken"),
            ],
        );
        assert_eq!(count, 2);
        assert_eq!(r.get("up_a").unwrap().source, "up");
        assert_eq!(r.get("taken").unwrap().source, "hub", "the first one stays");
        let count = r.replace_source("up", vec![double(true, "up_c"), double(true, "Bad")]);
        assert_eq!(count, 1);
        assert_eq!(r.deferred_names(), ["up_c"]);
        assert_eq!(r.search("double", 5).len(), 1, "the index follows");
    }

    #[test]
    fn to_text_shapes() {
        assert_eq!(ToolOutput::ok(1).to_text(), r#"{"result":1}"#);
        assert_eq!(
            ToolOutput::ok("a").with_next("b").to_text(),
            r#"{"result":"a","next":"b"}"#
        );
        assert_eq!(ToolOutput::error("boom").to_text(), r#"{"error":"boom"}"#);
    }
}

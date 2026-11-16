//! Tool registry: every tool (native, upstream, meta) goes through here.

use crate::config::is_valid_name;
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
    /// "hub" for native tools, otherwise the upstream server name.
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
        Self { value, next: None, is_error: false }
    }

    pub fn with_next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(next.into());
        self
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self { value: Value::String(msg.into()), next: None, is_error: true }
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
    let schema = serde_json::to_value(schemars::schema_for!(A)).unwrap_or(json!({"type": "object"}));
    let def = ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: schema,
        deferred,
        source: "hub".to_string(),
    };
    (def, Arc::new(Typed { f, _types: PhantomData }))
}

type Entry = (ToolDef, Arc<dyn ToolHandler>);

#[derive(Default)]
pub struct Registry {
    tools: RwLock<BTreeMap<String, Entry>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, def: ToolDef, handler: Arc<dyn ToolHandler>) -> anyhow::Result<()> {
        if !is_valid_name(&def.name) {
            bail!("tool name {:?} must match ^[a-z][a-z0-9_]{{0,63}}$", def.name);
        }
        let mut tools = self.tools.write().expect("registry lock");
        if tools.contains_key(&def.name) {
            bail!("tool {:?} is already registered", def.name);
        }
        tools.insert(def.name.clone(), (def, handler));
        Ok(())
    }

    pub fn unregister_source(&self, source: &str) {
        self.tools.write().expect("registry lock").retain(|_, (d, _)| d.source != source);
    }

    pub fn visible(&self) -> Vec<ToolDef> {
        let tools = self.tools.read().expect("registry lock");
        tools.values().filter(|(d, _)| !d.deferred).map(|(d, _)| d.clone()).collect()
    }

    pub fn deferred_names(&self) -> Vec<String> {
        let tools = self.tools.read().expect("registry lock");
        tools.values().filter(|(d, _)| d.deferred).map(|(d, _)| d.name.clone()).collect()
    }

    pub fn get(&self, name: &str) -> Option<ToolDef> {
        self.tools.read().expect("registry lock").get(name).map(|(d, _)| d.clone())
    }

    pub async fn call(&self, name: &str, args: Value) -> ToolOutput {
        let handler = self.tools.read().expect("registry lock").get(name).map(|(_, h)| h.clone());
        match handler {
            Some(h) => h.call(args).await,
            None => ToolOutput::error(format!(
                "unknown tool {name:?}; call search_tools with your need to find the right one"
            )),
        }
    }
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
        typed_tool(name, "Double a number.", deferred, |a: A| async move { ToolOutput::ok(a.n * 2) })
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
    fn unregister_source_removes_only_that_source() {
        let r = Registry::new();
        let (mut d, h) = double(true, "up_one");
        d.source = "up".into();
        r.register(d, h).unwrap();
        let (d, h) = double(false, "mine");
        r.register(d, h).unwrap();
        r.unregister_source("up");
        assert!(r.deferred_names().is_empty());
        assert_eq!(r.visible().len(), 1);
    }

    #[test]
    fn to_text_shapes() {
        assert_eq!(ToolOutput::ok(1).to_text(), r#"{"result":1}"#);
        assert_eq!(ToolOutput::ok("a").with_next("b").to_text(), r#"{"result":"a","next":"b"}"#);
        assert_eq!(ToolOutput::error("boom").to_text(), r#"{"error":"boom"}"#);
    }
}

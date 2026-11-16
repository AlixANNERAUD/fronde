//! The `tool` world: components that bring tools to the hub's registry.

use crate::ComponentSpec;
use crate::host::{ComponentHost, Sandbox};
use anyhow::{Context, bail};
use async_trait::async_trait;
use jarvis_hub::tools::{Registry, ToolDef, ToolHandler, ToolOutput};
use serde_json::Value;
use std::sync::Arc;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "jarvis:component/tool",
        // Exports run guest code that waits on HTTP.
        exports: { default: async },
        with: { "jarvis:component/settings": crate::host::bindings::jarvis::component::settings },
    });
}

use bindings::ToolPre;

const TOOLS_EXPORT: &str = "jarvis:component/tools@";

/// A compiled tool component and the tools it declared when it loaded.
#[derive(Clone)]
pub struct LoadedComponent {
    inner: Arc<Loaded>,
}

struct Loaded {
    sandbox: Sandbox,
    pre: ToolPre<crate::host::HostState>,
    tools: Vec<ToolDef>,
}

impl LoadedComponent {
    /// Compile the component, check its imports and exports, and ask it for its tools.
    pub async fn load(host: &ComponentHost, spec: ComponentSpec) -> anyhow::Result<Self> {
        let component = host.compile(&spec).await?;
        let exports_tools = component
            .component_type()
            .exports(host.engine())
            .any(|(name, _)| name.starts_with(TOOLS_EXPORT));
        if !exports_tools {
            bail!(
                "component {} does not export jarvis:component/tools: build it for the jarvis:component/tool world",
                spec.name
            );
        }
        let pre = ToolPre::new(host.linker().instantiate_pre(&component)?)?;
        let sandbox = Sandbox::new(host, spec)?;
        let (mut store, out_of_cpu) = sandbox.store();
        let call = async {
            let instance = pre.instantiate_async(&mut store).await?;
            instance
                .jarvis_component_tools()
                .call_list_tools(&mut store)
                .await
        };
        let declared = sandbox
            .run(&out_of_cpu, call)
            .await
            .map_err(anyhow::Error::msg)?;
        let tools = declared
            .into_iter()
            .map(|t| {
                let input_schema = serde_json::from_str(&t.input_schema).with_context(|| {
                    format!(
                        "tool {} of {} has an invalid input schema",
                        t.name, sandbox.name
                    )
                })?;
                Ok(ToolDef {
                    name: t.name,
                    description: t.description,
                    input_schema,
                    deferred: t.deferred,
                    source: sandbox.name.clone(),
                })
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            inner: Arc::new(Loaded {
                sandbox,
                pre,
                tools,
            }),
        })
    }

    pub fn name(&self) -> &str {
        &self.inner.sandbox.name
    }

    pub fn list_tools(&self) -> &[ToolDef] {
        &self.inner.tools
    }

    pub async fn call(&self, name: &str, args: &Value) -> ToolOutput {
        let Loaded { sandbox, pre, .. } = &*self.inner;
        let arguments = args.to_string();
        let (mut store, out_of_cpu) = sandbox.store();
        let call = async {
            let instance = pre.instantiate_async(&mut store).await?;
            instance
                .jarvis_component_tools()
                .call_call_tool(&mut store, name, &arguments)
                .await
        };
        match sandbox.run(&out_of_cpu, call).await {
            Ok(out) => ToolOutput {
                value: serde_json::from_str(&out.value).unwrap_or(Value::String(out.value)),
                next: out.next,
                is_error: out.is_error,
            },
            Err(message) => ToolOutput::error(message),
        }
    }

    /// Add the component's tools to the registry, each calling the component.
    pub fn register(&self, registry: &Registry) -> anyhow::Result<()> {
        for def in self.list_tools() {
            let handler = Arc::new(Handler {
                component: self.clone(),
                tool: def.name.clone(),
            });
            registry.register(def.clone(), handler)?;
        }
        Ok(())
    }
}

struct Handler {
    component: LoadedComponent,
    tool: String,
}

#[async_trait]
impl ToolHandler for Handler {
    async fn call(&self, args: Value) -> ToolOutput {
        self.component.call(&self.tool, &args).await
    }
}

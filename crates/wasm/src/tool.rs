//! The `tool` world: components that bring tools to the hub's registry.

use crate::ComponentSpec;
use crate::api::Api;
use crate::host::{ComponentHost, HostState, Sandbox};
use crate::v0_1;
use anyhow::{Context, bail};
use async_trait::async_trait;
use fronde_hub::tools::{Registry, ToolDef, ToolHandler, ToolOutput};
use serde_json::Value;
use std::sync::Arc;
use wasmtime::Store;

const TOOLS_EXPORT: &str = "fronde:component/tools@";

/// A tool as a component declares it, whatever its interface version.
pub(crate) struct DeclaredTool {
    pub(crate) name: String,
    pub(crate) description: String,
    /// JSON text.
    pub(crate) input_schema: String,
    pub(crate) deferred: bool,
}

/// What a tool call returns, whatever the interface version.
pub(crate) struct CallOutput {
    /// JSON text, or plain text.
    pub(crate) value: String,
    pub(crate) next: Option<String>,
    pub(crate) is_error: bool,
}

/// The component's exports, through the bindings of the version it was built against.
enum Tools {
    V0_1(v0_1::tool::Tools),
}

impl Tools {
    async fn list(&self, store: &mut Store<HostState>) -> wasmtime::Result<Vec<DeclaredTool>> {
        match self {
            Self::V0_1(tools) => tools.list(store).await,
        }
    }

    async fn call(
        &self,
        store: &mut Store<HostState>,
        name: &str,
        arguments: &str,
    ) -> wasmtime::Result<CallOutput> {
        match self {
            Self::V0_1(tools) => tools.call(store, name, arguments).await,
        }
    }
}

/// A compiled tool component and the tools it declared when it loaded.
#[derive(Clone)]
pub struct LoadedComponent {
    inner: Arc<Loaded>,
}

struct Loaded {
    sandbox: Sandbox,
    exports: Tools,
    tools: Vec<ToolDef>,
}

impl LoadedComponent {
    /// Compile the component, check its imports and exports, and ask it for its tools.
    pub async fn load(host: &ComponentHost, spec: ComponentSpec) -> anyhow::Result<Self> {
        let component = host.compile(&spec).await?;
        let api = Api::of(&component, host.engine(), &spec.name)?;
        let exports_tools = component
            .component_type()
            .exports(host.engine())
            .any(|(name, _)| name.starts_with(TOOLS_EXPORT));
        let (Some(api), true) = (api, exports_tools) else {
            bail!(
                "component {} does not export fronde:component/tools: build it for the fronde:component/tool world",
                spec.name
            );
        };
        let exports = match api {
            Api::V0_1 => Tools::V0_1(v0_1::tool::Tools::new(host.linker(), &component)?),
        };
        let sandbox = Sandbox::new(host, spec)?;
        let (mut store, out_of_cpu) = sandbox.store();
        let declared = sandbox
            .run(&out_of_cpu, exports.list(&mut store))
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
                exports,
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
        let Loaded {
            sandbox, exports, ..
        } = &*self.inner;
        let arguments = args.to_string();
        let (mut store, out_of_cpu) = sandbox.store();
        let call = exports.call(&mut store, name, &arguments);
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

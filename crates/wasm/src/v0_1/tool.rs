//! The `tool` world of `fronde:component@0.1`.

use crate::host::HostState;
use crate::tool::{CallOutput, DeclaredTool};
use wasmtime::Store;
use wasmtime::component::{Component, Linker};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/v0.1",
        world: "fronde:component/tool",
        // Exports run guest code that waits on HTTP.
        exports: { default: async },
        with: { "fronde:component/settings": crate::v0_1::shared::fronde::component::settings },
    });
}

/// A component of this version, ready to instantiate for each call.
pub(crate) struct Tools(bindings::ToolPre<HostState>);

impl Tools {
    pub(crate) fn new(linker: &Linker<HostState>, component: &Component) -> anyhow::Result<Self> {
        Ok(Self(bindings::ToolPre::new(
            linker.instantiate_pre(component)?,
        )?))
    }

    pub(crate) async fn list(
        &self,
        store: &mut Store<HostState>,
    ) -> wasmtime::Result<Vec<DeclaredTool>> {
        let instance = self.0.instantiate_async(&mut *store).await?;
        let tools = instance
            .fronde_component_tools()
            .call_list_tools(store)
            .await?;
        Ok(tools
            .into_iter()
            .map(|t| DeclaredTool {
                name: t.name,
                description: t.description,
                input_schema: t.input_schema,
                deferred: t.deferred,
            })
            .collect())
    }

    pub(crate) async fn call(
        &self,
        store: &mut Store<HostState>,
        name: &str,
        arguments: &str,
    ) -> wasmtime::Result<CallOutput> {
        let instance = self.0.instantiate_async(&mut *store).await?;
        let out = instance
            .fronde_component_tools()
            .call_call_tool(store, name, arguments)
            .await?;
        Ok(CallOutput {
            value: out.value,
            next: out.next,
            is_error: out.is_error,
        })
    }
}

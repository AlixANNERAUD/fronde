//! Write tool components for [Fronde](https://github.com/AlixANNERAUD/fronde): WebAssembly
//! components (`wasm32-wasip2`) that bring tools to its hub.
//!
//! Implement [`Tools`] and export it with [`export_tool!`]; build with
//! `cargo build --release --target wasm32-wasip2` and a `cdylib` crate type.
//!
//! ```
//! use fronde_component::{Tool, ToolOutput, Tools, export_tool};
//! use serde_json::json;
//!
//! struct Hello;
//!
//! impl Tools for Hello {
//!     fn list_tools() -> Vec<Tool> {
//!         vec![Tool::new("hello", "Greet someone.", json!({
//!             "type": "object",
//!             "properties": {"name": {"type": "string"}},
//!         }))]
//!     }
//!
//!     fn call_tool(name: &str, arguments: &str) -> ToolOutput {
//!         match name {
//!             "hello" => ToolOutput::ok(format!("hello {arguments}")),
//!             other => ToolOutput::error(format!("unknown tool {other}")),
//!         }
//!     }
//! }
//!
//! export_tool!(Hello);
//! ```
//!
//! This release targets `fronde:component@0.1` (see [`API`]); the WIT ships in the crate's `wit`
//! directory. A component reads its [`settings`] and reaches the network through [`http`], only
//! to the origins the host allows, which adds the credentials itself: a component never holds a
//! secret. The `testing` feature brings fakes to test a component natively.

#![deny(unsafe_code)]

pub mod http;
pub mod settings;
#[cfg(feature = "testing")]
pub mod testing;

use serde::Serialize;
use serde_json::Value;

/// The `fronde:component` version this SDK targets, as a component manifest's `api` names it.
pub const API: &str = "0.1";

/// The async runtime of the component's calls, for HTTP and timers.
#[cfg(target_arch = "wasm32")]
pub use wstd;

/// Run a future to completion inside a call, for instance to send HTTP requests.
#[cfg(target_arch = "wasm32")]
pub fn block_on<F: Future>(future: F) -> F::Output {
    wstd::runtime::block_on(future)
}

/// A tool the component brings.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    /// Matches `^[a-z][a-z0-9_]{0,63}$`.
    pub name: String,
    /// A short imperative rule for the model.
    pub description: String,
    /// JSON Schema of the arguments.
    pub input_schema: Value,
    /// Hidden from the tool list; found through `search_tools`.
    pub deferred: bool,
}

impl Tool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            deferred: false,
        }
    }

    /// Hide the tool from the list: the model finds it through `search_tools`.
    pub fn deferred(mut self) -> Self {
        self.deferred = true;
        self
    }
}

/// The result of a tool call: JSON for the model, or an error message that says how to fix it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub value: Value,
    pub next: Option<String>,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(value: impl Serialize) -> Self {
        let value = serde_json::to_value(value).unwrap_or_else(|e| Value::String(e.to_string()));
        Self {
            value,
            next: None,
            is_error: false,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            value: Value::String(message.into()),
            next: None,
            is_error: true,
        }
    }
}

/// What a component of the `tool` world exports. Each call runs in a fresh instance: nothing is
/// kept from one call to the next.
pub trait Tools {
    /// Called once, when the host loads the component.
    fn list_tools() -> Vec<Tool>;
    /// `arguments` is JSON text.
    fn call_tool(name: &str, arguments: &str) -> ToolOutput;
}

/// Export a type that implements [`Tools`] as the component's `fronde:component/tools`.
///
/// Natively it only checks the type, so that a component's crate also builds for its tests.
#[cfg(target_arch = "wasm32")]
#[macro_export]
macro_rules! export_tool {
    ($ty:ty) => {
        const _: () = {
            struct Export;
            impl $crate::bindings::exports::fronde::component::tools::Guest for Export {
                fn list_tools()
                -> ::std::vec::Vec<$crate::bindings::exports::fronde::component::tools::Tool>
                {
                    $crate::__private::list_tools::<$ty>()
                }
                fn call_tool(
                    name: ::std::string::String,
                    arguments: ::std::string::String,
                ) -> $crate::bindings::exports::fronde::component::tools::ToolOutput {
                    $crate::__private::call_tool::<$ty>(&name, &arguments)
                }
            }
    $crate::__export_tool_impl!(Export with_types_in $crate::bindings);
        };
    };
}

/// Export a type that implements [`Tools`] as the component's `fronde:component/tools`.
///
/// Natively it only checks the type, so that a component's crate also builds for its tests.
#[cfg(not(target_arch = "wasm32"))]
#[macro_export]
macro_rules! export_tool {
    ($ty:ty) => {
        const _: fn() = || {
            fn exported<T: $crate::Tools>() {}
            exported::<$ty>();
        };
    };
}

// The generated import and export glue needs `unsafe`; nothing else in the crate may use it.
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
#[allow(unsafe_code)]
pub mod bindings {
    wit_bindgen::generate!({
        world: "tool",
        path: "wit",
        pub_export_macro: true,
        default_bindings_module: "$crate::bindings",
    });
}

#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub mod __private {
    use crate::bindings::exports::fronde::component::tools as wit;

    pub fn list_tools<T: crate::Tools>() -> Vec<wit::Tool> {
        T::list_tools()
            .into_iter()
            .map(|t| wit::Tool {
                name: t.name,
                description: t.description,
                input_schema: t.input_schema.to_string(),
                deferred: t.deferred,
            })
            .collect()
    }

    pub fn call_tool<T: crate::Tools>(name: &str, arguments: &str) -> wit::ToolOutput {
        let out = T::call_tool(name, arguments);
        wit::ToolOutput {
            value: out.value.to_string(),
            next: out.next,
            is_error: out.is_error,
        }
    }
}

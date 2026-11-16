//! Calendar component: calendar_* and reminders_* tools over CalDAV.
//!
//! Settings: `events_url` and `reminders_url` (CalDAV collections, required) and `timezone` (IANA,
//! default UTC). Requests carry no credentials: the host adds them for the origins it allows.
//! The logic modules build on any target, so their tests run on the host; only the WIT glue
//! below is for wasm32.

#![deny(unsafe_code)]

pub mod backend;
pub mod caldav;
pub mod ics;
pub mod tools;

#[cfg(test)]
mod testing;

#[cfg(target_arch = "wasm32")]
mod guest {
    // The generated export glue needs `unsafe`; nothing else in the crate may use it.
    #[allow(unsafe_code)]
    mod bindings {
        wit_bindgen::generate!({ world: "tool", path: "../../wit" });
        use super::Component;
        export!(Component with_types_in crate::guest::bindings);
    }

    use crate::backend::Settings;
    use crate::caldav::{Request, Response, Transport};
    use crate::tools::{self, Output};
    use bindings::exports::fronde::component::tools::{Guest, Tool, ToolOutput};
    use bindings::fronde::component::settings;
    use wstd::http::{Body, BodyExt, Client, Method};

    struct Component;

    impl Guest for Component {
        fn list_tools() -> Vec<Tool> {
            tools::definitions()
                .into_iter()
                .map(|t| Tool {
                    name: t.name.into(),
                    description: t.description.into(),
                    input_schema: t.input_schema.to_string(),
                    deferred: false,
                })
                .collect()
        }

        fn call_tool(name: String, arguments: String) -> ToolOutput {
            let out = match Settings::read(settings::get) {
                Ok(s) => wstd::runtime::block_on(tools::call(
                    &s.backend(WasiHttp),
                    s.tz,
                    &name,
                    &arguments,
                )),
                Err(e) => Output::error(e),
            };
            ToolOutput {
                value: out.value.to_string(),
                next: None,
                is_error: out.is_error,
            }
        }
    }

    /// WASI HTTP, through the host's allowlist; the host also sets the time limits.
    #[derive(Clone, Copy)]
    struct WasiHttp;

    impl Transport for WasiHttp {
        async fn send(&self, req: Request) -> anyhow::Result<Response> {
            let mut builder = wstd::http::Request::builder()
                .method(Method::from_bytes(req.method.as_bytes())?)
                .uri(req.url.as_str());
            for (name, value) in req.headers {
                builder = builder.header(name, value);
            }
            let resp = Client::new()
                .send(builder.body(Body::from(req.body))?)
                .await?;
            let status = resp.status().as_u16();
            let body = resp
                .into_body()
                .into_boxed_body()
                .collect()
                .await?
                .to_bytes();
            Ok(Response {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            })
        }
    }
}

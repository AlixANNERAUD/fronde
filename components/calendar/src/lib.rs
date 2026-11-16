//! Calendar component: calendar_* and reminders_* tools over CalDAV.
//!
//! Settings: `events_url` and `reminders_url` (CalDAV collections, required) and `timezone` (IANA,
//! default UTC). Requests carry no credentials: the host adds them for the origins it allows.
//! The logic builds on any target, so its tests run on the host; only the export glue below is
//! for wasm32.

pub mod backend;
pub mod caldav;
pub mod ics;
pub mod tools;

#[cfg(test)]
mod testing;

/// The export glue: tools that read the settings and reach CalDAV over WASI HTTP.
#[cfg(target_arch = "wasm32")]
mod guest {
    use crate::backend::Settings;
    use crate::tools::{self, Output};
    use fronde_component::http::WasiHttp;
    use fronde_component::{Tool, ToolOutput, Tools, export_tool, settings};

    struct Calendar;

    export_tool!(Calendar);

    impl Tools for Calendar {
        fn list_tools() -> Vec<Tool> {
            tools::definitions()
                .into_iter()
                .map(|t| Tool::new(t.name, t.description, t.input_schema))
                .collect()
        }

        fn call_tool(name: &str, arguments: &str) -> ToolOutput {
            let out = match Settings::read(settings::get) {
                Ok(s) => fronde_component::block_on(tools::call(
                    &s.backend(WasiHttp),
                    s.tz,
                    name,
                    arguments,
                )),
                Err(e) => Output::error(e),
            };
            ToolOutput {
                value: out.value,
                next: None,
                is_error: out.is_error,
            }
        }
    }
}

//! `fronde:component@0.1`, generated from `wit/v0.1`. Frozen like that directory: a later version
//! gets a module of its own beside this one, and this one keeps serving the components built for
//! it.

pub(crate) mod tool;

use crate::host::HostState;
use wasmtime::component::{HasSelf, Linker};

/// What every world of this version imports; each world's bindgen reuses it through `with`.
pub(crate) mod shared {
    wasmtime::component::bindgen!({
        path: "../../wit/v0.1",
        inline: "package fronde:host; world shared { import fronde:component/settings@0.1.0; }",
        world: "fronde:host/shared",
    });
}

/// Define this version's host interfaces.
pub(crate) fn add_to_linker(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    shared::fronde::component::settings::add_to_linker::<_, HasSelf<HostState>>(linker, |s| s)?;
    Ok(())
}

impl shared::fronde::component::settings::Host for HostState {
    fn get(&mut self, key: String) -> Option<String> {
        self.setting(&key)
    }
}

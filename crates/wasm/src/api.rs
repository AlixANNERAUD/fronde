//! Which version of `fronde:component` a component was built against, among those this host
//! serves. Each version has a frozen WIT directory (`wit/v0.1`, …) and a module of host bindings.

use anyhow::bail;
use wasmtime::Engine;
use wasmtime::component::Component;

const PACKAGE: &str = "fronde:component/";

/// A version of `fronde:component` this host serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    V0_1,
}

impl Api {
    pub const ALL: [Self; 1] = [Self::V0_1];

    /// `major.minor`, as a component's manifest names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V0_1 => "0.1",
        }
    }

    /// The version of the `fronde:component` interfaces the component imports and exports, or
    /// `None` when it uses none. Refuses a version this host does not serve, or a mix.
    pub(crate) fn of(
        component: &Component,
        engine: &Engine,
        name: &str,
    ) -> anyhow::Result<Option<Self>> {
        let ty = component.component_type();
        let names = ty
            .imports(engine)
            .map(|(n, _)| n)
            .chain(ty.exports(engine).map(|(n, _)| n));
        let mut found = None;
        for item in names {
            let Some(interface) = item.strip_prefix(PACKAGE) else {
                continue;
            };
            let version = interface.split_once('@').map(|(_, v)| v);
            let Some(api) = version.and_then(Self::serving) else {
                let built = version.map_or("without a version".to_string(), |v| {
                    format!("built for fronde:component@{v}")
                });
                bail!(
                    "component {name} uses {item}, {built}, but this Fronde serves {}: rebuild \
                     it with a fronde-component release for one of those",
                    served()
                );
            };
            if found.is_some_and(|f| f != api) {
                bail!("component {name} mixes versions of fronde:component ({item})");
            }
            found = Some(api);
        }
        Ok(found)
    }

    /// Versions are compatible within `major.minor` while below 1.0, as Cargo reads them.
    fn serving(version: &str) -> Option<Self> {
        let mut parts = version.split('.');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some("0"), Some("1"), Some(patch), None) if patch.parse::<u32>().is_ok() => {
                Some(Self::V0_1)
            }
            _ => None,
        }
    }
}

/// "0.1", or "0.1, 0.2".
pub(crate) fn served() -> String {
    Api::ALL.map(Api::as_str).join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_patch_releases_of_its_versions() {
        assert_eq!(Api::serving("0.1.0"), Some(Api::V0_1));
        assert_eq!(Api::serving("0.1.7"), Some(Api::V0_1));
        for other in ["0.2.0", "1.0.0", "0.1", "", "0.1.0-rc1", "0.1.0.1"] {
            assert_eq!(Api::serving(other), None, "{other}");
        }
    }
}

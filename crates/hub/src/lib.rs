//! Hub: tools, memory, tool sources and the MCP server.

pub mod config;
pub mod guide;
pub mod mcp;
pub mod memory;
pub mod origin;
pub mod private;
pub mod search;
pub mod source;
pub mod tools;
pub mod upstream;
pub mod user;

use config::{Config, SourceConfig};
use fronde_db::DatabaseConnection;
use fronde_requests::Requests;
use memory::index::MemoryIndex;
use memory::{MemoryStore, tools::memory_tools};
use source::{Native, Supervisor, ToolSource};
use std::sync::Arc;
use tools::{Registry, meta_tools};
use upstream::Upstream;

/// Everything the hub serves, assembled from the configuration.
pub struct Hub {
    pub registry: Arc<Registry>,
    pub memory: Arc<MemoryStore>,
    pub sources: Supervisor,
    pub config: Config,
    /// Fronde's database (`fronde_db::open`), shared with the rest of the server.
    pub db: DatabaseConnection,
}

impl Hub {
    /// Open memory, register the hub's own tools, start `components` (one that fails is skipped
    /// with a warning, unless it is waiting for a stored secret: that one is tried again) and
    /// connect upstream servers in the background. With `requests` (the chat runs), the `user_*`
    /// tools ask the user through it.
    pub async fn start(
        config: Config,
        db: DatabaseConnection,
        components: Vec<Arc<dyn ToolSource>>,
        requests: Option<Arc<Requests>>,
    ) -> anyhow::Result<Self> {
        let dir = &config.data.dir;
        private::prepare_data_dir(dir)?;
        let memory = Arc::new(MemoryStore::open(
            &dir.join("memory"),
            &config.memory,
            config.data.timezone,
        )?);
        remove_legacy_index(dir);
        let index = Arc::new(MemoryIndex::new(db.clone()));
        index.reindex_all(memory.dir()).await?;

        let registry = Arc::new(Registry::new());
        let sources = Supervisor::new(registry.clone());
        let mut own = meta_tools(&registry);
        own.extend(memory_tools(memory.clone(), index, config.memory.git));
        if let Some(requests) = requests {
            own.extend(user::user_tools(requests));
        }
        sources.start(Arc::new(Native::new("hub", own))).await?;
        for component in components {
            let name = component.name().to_string();
            match sources.start(component).await {
                Err(e) if e.downcast_ref::<source::Waiting>().is_some() => {
                    tracing::warn!("{name}: not started yet, trying again: {e:#}");
                }
                Err(e) => tracing::warn!("{name}: skipped: {e:#}"),
                Ok(()) => {}
            }
        }
        for source in &config.sources {
            if let SourceConfig::Mcp(m) = source {
                sources.spawn(Arc::new(Upstream::new(m.clone())));
            }
        }
        Ok(Self {
            registry,
            memory,
            sources,
            config,
            db,
        })
    }
}

/// Delete the memory index file of Fronde versions before the database (`index.sqlite`, and the
/// journal a crash may have left). It was a cache of the memory files, now in `fronde.db`.
fn remove_legacy_index(dir: &std::path::Path) {
    for name in ["index.sqlite", "index.sqlite-journal"] {
        let path = dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!(
                "deleted {}: the memory index is now in the database",
                path.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                "cannot delete {}: {e}; it is no longer used, delete it by hand",
                path.display()
            ),
        }
    }
}

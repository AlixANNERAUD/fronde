//! Hub: tools, memory, calendar and MCP server.

pub mod calendar;
pub mod config;
pub mod guide;
pub mod mcp;
pub mod memory;
pub mod private;
pub mod search;
pub mod source;
pub mod tools;
pub mod upstream;

use config::{Config, SourceConfig};
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
}

impl Hub {
    /// Open memory, register the hub's own tools, start `components` (one that fails is skipped
    /// with a warning) and connect upstream servers in the background.
    pub async fn start(
        config: Config,
        components: Vec<Arc<dyn ToolSource>>,
    ) -> anyhow::Result<Self> {
        let dir = &config.data.dir;
        private::prepare_data_dir(dir)?;
        let memory = Arc::new(MemoryStore::open(
            &dir.join("memory"),
            &config.memory,
            config.data.timezone,
        )?);
        let index = Arc::new(MemoryIndex::open(&dir.join("index.sqlite"))?);
        index.reindex_all(memory.dir())?;

        let registry = Arc::new(Registry::new());
        let sources = Supervisor::new(registry.clone());
        let mut own = meta_tools(&registry);
        own.extend(memory_tools(memory.clone(), index, config.memory.git));
        sources.start(Arc::new(Native::new("hub", own))).await?;
        for component in components {
            let name = component.name().to_string();
            if let Err(e) = sources.start(component).await {
                tracing::warn!("{name}: skipped: {e:#}");
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
        })
    }
}

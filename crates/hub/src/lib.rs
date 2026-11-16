//! Hub: tools, memory, calendar and MCP server.

pub mod calendar;
pub mod config;
pub mod guide;
pub mod mcp;
pub mod memory;
pub mod search;
pub mod tools;
pub mod upstream;

use anyhow::Context;
use calendar::caldav::CalDav;
use calendar::{CalDavBackend, tools::register_calendar_tools};
use config::Config;
use memory::index::MemoryIndex;
use memory::{MemoryStore, tools::register_memory_tools};
use std::sync::Arc;
use tools::{Registry, register_meta_tools};

/// Everything the hub serves, assembled from the configuration.
pub struct Hub {
    pub registry: Arc<Registry>,
    pub memory: Arc<MemoryStore>,
    pub config: Config,
}

impl Hub {
    /// Open memory, register native tools and start upstream connections (in the background).
    pub async fn start(config: Config) -> anyhow::Result<Self> {
        let dir = &config.data.dir;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create data dir {}", dir.display()))?;
        let memory = Arc::new(MemoryStore::open(
            &dir.join("memory"),
            &config.memory,
            config.data.timezone,
        )?);
        let index = Arc::new(MemoryIndex::open(&dir.join("index.sqlite"))?);
        index.reindex_all(memory.dir())?;

        let registry = Arc::new(Registry::new());
        register_meta_tools(&registry)?;
        register_memory_tools(&registry, memory.clone(), index, config.memory.git)?;
        if let Some(c) = &config.calendar {
            let password = c.password.resolve().context("calendar.password")?;
            let backend = CalDavBackend {
                events: CalDav::new(c.events_url.clone(), c.username.clone(), password.clone()),
                reminders: CalDav::new(c.reminders_url.clone(), c.username.clone(), password),
                tz: config.data.timezone,
            };
            register_calendar_tools(&registry, Arc::new(backend), config.data.timezone)?;
        }
        for m in &config.mcp {
            upstream::spawn_upstream(m.clone(), registry.clone());
        }
        Ok(Self {
            registry,
            memory,
            config,
        })
    }
}

//! Hub: tools, memory, tool sources and the MCP server.

pub mod config;
pub mod guide;
pub mod integrations;
pub mod mcp;
pub mod memory;
pub mod origin;
pub mod private;
pub mod search;
pub mod source;
pub mod stored;
pub mod threads;
pub mod tools;
pub mod upstream;
pub mod user;

use config::{Config, SourceConfig};
use fronde_db::DatabaseConnection;
use fronde_requests::Requests;
use integrations::Integrations;
use memory::index::MemoryIndex;
use memory::{MemoryStore, tools::memory_tools};
use source::{Native, Supervisor, ToolSource};
use std::sync::Arc;
use threads::ThreadControl;
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
    /// The threads the `threads_*` tools drive, when the chat runs.
    pub threads: Option<Arc<dyn ThreadControl>>,
}

impl Hub {
    /// Open memory, register the hub's own tools, start `components` (one that fails is skipped
    /// with a warning, unless it is waiting for a stored secret: that one is tried again) and
    /// connect upstream servers in the background. With `requests` (the chat runs), the `user_*`
    /// tools ask the user through it; with `threads`, the `threads_*` tools let a thread's agent
    /// start and follow sub-agents; with both and `integrations`, `integrations_request_setup`
    /// lets a thread's agent ask the admin to set up a shipped component, which `search_tools`
    /// lists to the threads' agents.
    pub async fn start(
        config: Config,
        db: DatabaseConnection,
        components: Vec<Arc<dyn ToolSource>>,
        requests: Option<Arc<Requests>>,
        threads: Option<Arc<dyn ThreadControl>>,
        integrations: Option<Arc<dyn Integrations>>,
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
        // Set up from a card only where a card can be answered (the chat's requests) and where
        // the threads check the turn that asks: without them, a turn a sub-agent's result
        // started would not be refused.
        let integrations = integrations.filter(|_| requests.is_some() && threads.is_some());
        let mut own = meta_tools(&registry, integrations.clone());
        own.extend(memory_tools(memory.clone(), index, config.memory.git));
        // Upstream servers' forms go to the admin; the one user so far is the owner, an admin.
        let asking = requests.as_ref().map(|requests| {
            upstream::Asking::new(
                requests.clone(),
                config.data.user.clone(),
                mcp::TOPIC.to_owned(),
            )
        });
        if let Some(requests) = requests {
            if let Some(integrations) = integrations {
                own.extend(integrations::integration_tools(integrations::Setup {
                    requests: requests.clone(),
                    threads: threads.clone(),
                    integrations,
                    sources: sources.clone(),
                    registry: registry.clone(),
                    admin: config.data.user.clone(),
                    setting_up: integrations::SettingUp::default(),
                }));
            }
            own.extend(user::user_tools(requests, threads.clone()));
        }
        if let Some(threads) = &threads {
            own.extend(threads::thread_tools(threads.clone()));
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
        // The master key's place is needed only by a server whose token is stored (a
        // configuration not read by `config::load` has none).
        let stored_token =
            |m: &config::UpstreamConfig| matches!(m.token, Some(config::Secret::Stored { .. }));
        let backends = if config
            .sources
            .iter()
            .any(|s| matches!(s, SourceConfig::Mcp(m) if stored_token(m)))
        {
            Some(config.master_key_backends()?)
        } else {
            None
        };
        for source in &config.sources {
            if let SourceConfig::Mcp(m) = source {
                let mut upstream = Upstream::asking(m.clone(), asking.clone());
                if let Some(backends) = &backends {
                    upstream = upstream.reading_secrets_from(db.clone(), backends.clone());
                }
                sources.spawn(Arc::new(upstream));
            }
        }
        Ok(Self {
            registry,
            memory,
            sources,
            config,
            db,
            threads,
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

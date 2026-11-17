//! Threads that only show cards: a thread's card goes straight to the requests, as the threads
//! show it during a running turn of the owner's. Nothing else is reached; the tests that need
//! the threads' own checks (a wake turn, a sub-agent) run the real ones in `threads.rs`.

use async_trait::async_trait;
use fronde_hub::threads::{
    Child, NewChild, ProviderModels, ReadThread, Started, ThreadControl, ThreadError, Waited,
};
use fronde_protocol::ThreadId;
use fronde_requests::{Ask, Requests, Waiter};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct Asks(pub Arc<Requests>);

#[async_trait]
impl ThreadControl for Asks {
    async fn delegation(&self, _: ThreadId) -> Result<(), ThreadError> {
        Err(ThreadError::Refused("no sub-agents here".into()))
    }

    async fn providers(&self) -> Result<Vec<ProviderModels>, ThreadError> {
        Ok(Vec::new())
    }

    async fn start(
        &self,
        _: ThreadId,
        _: Option<u64>,
        _: NewChild,
    ) -> Result<Started, ThreadError> {
        Err(ThreadError::Refused("no sub-agents here".into()))
    }

    async fn send(
        &self,
        _: ThreadId,
        _: Option<u64>,
        _: ThreadId,
        _: String,
    ) -> Result<u64, ThreadError> {
        Err(ThreadError::NoSuchThread)
    }

    async fn read(&self, _: ThreadId, _: ThreadId, _: u64) -> Result<ReadThread, ThreadError> {
        Err(ThreadError::NoSuchThread)
    }

    async fn children(&self, _: ThreadId) -> Result<(Vec<Child>, usize), ThreadError> {
        Ok((Vec::new(), 0))
    }

    async fn cancel(
        &self,
        _: ThreadId,
        _: ThreadId,
        _: Option<String>,
    ) -> Result<bool, ThreadError> {
        Err(ThreadError::NoSuchThread)
    }

    async fn wait(
        &self,
        _: ThreadId,
        _: Option<u64>,
        _: ThreadId,
        _: u64,
        _: Duration,
        _: CancellationToken,
    ) -> Result<Waited, ThreadError> {
        Err(ThreadError::NoSuchThread)
    }

    async fn ask(&self, _: ThreadId, ask: Ask) -> Result<Option<Waiter>, ThreadError> {
        self.0.ask(ask).await.map(Some).map_err(ThreadError::Failed)
    }
}

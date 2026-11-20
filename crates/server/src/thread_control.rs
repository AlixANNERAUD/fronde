//! The bridge between the hub's `threads_*` tools and the threads: the hub keeps no dependency
//! on `fronde-agent`, so the server hands it [`ThreadBridge`] over [`Threads`].

use async_trait::async_trait;
use fronde_agent::assistant::{AdminFromMember, AdminFromSubAgent, LastingInWake, SecretInWake};
use fronde_agent::threads::{
    Background, ChildEnd, ChildTask, Delegation, ThreadId, Threads, Unbound,
};
use fronde_hub::threads::{
    Child, NewChild, ProviderModels, ReadThread, Started, ThreadControl, ThreadError, Waited,
};
use fronde_requests::{Ask, Waiter};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// [`ThreadControl`] over the threads: every check of what a caller may reach is theirs.
pub struct ThreadBridge(pub Arc<Threads>);

/// What the hub tells the agent of `error`: a refusal as the threads word it, an unknown
/// thread and one out of reach alike, anything else a failure logged by the hub.
fn hub_error(error: anyhow::Error) -> ThreadError {
    if let Some(refused) = error.downcast_ref::<SecretInWake>() {
        return ThreadError::Refused(refused.to_string());
    }
    if let Some(refused) = error.downcast_ref::<LastingInWake>() {
        return ThreadError::Refused(refused.to_string());
    }
    if let Some(refused) = error.downcast_ref::<AdminFromSubAgent>() {
        return ThreadError::Refused(refused.to_string());
    }
    if let Some(refused) = error.downcast_ref::<AdminFromMember>() {
        return ThreadError::Refused(refused.to_string());
    }
    match error.downcast_ref::<Delegation>() {
        Some(Delegation::NoSuchThread) => ThreadError::NoSuchThread,
        Some(refused) => ThreadError::Refused(refused.to_string()),
        None => ThreadError::Failed(error),
    }
}

/// Whether background work may run, as `threads_list` tells an agent before it tries.
pub fn background_text(background: &Background) -> String {
    match background {
        Background::Allowed { .. } => "allowed".to_owned(),
        Background::Paused(pause) => match pause.resets_at {
            Some(at) => format!(
                "paused until {}",
                fronde_protocol::usage::ceil_minute(at).format("%Y-%m-%d %H:%M UTC")
            ),
            None => "paused".to_owned(),
        },
        Background::Unknown => "unknown".to_owned(),
    }
}

#[async_trait]
impl ThreadControl for ThreadBridge {
    async fn delegation(&self, caller: ThreadId) -> Result<(), ThreadError> {
        self.0.check_delegate(caller).await.map_err(hub_error)
    }

    async fn providers(&self, caller: ThreadId) -> Result<Vec<ProviderModels>, ThreadError> {
        let Some(caller) = self.0.get(caller).await.map_err(hub_error)? else {
            return Err(ThreadError::NoSuchThread);
        };
        let mut providers = Vec::new();
        for id in self.0.providers() {
            // The account a sub-agent of the caller would spend on this provider: the caller's
            // own on its provider, its owner's default on another. None, and no sub-agent runs
            // there.
            let chosen = caller.account.as_deref().filter(|_| caller.provider == id);
            let resolved = self.0.resolve_on(&caller.owner, id, chosen);
            let background = match &resolved {
                Ok(account) => background_text(&self.0.background(account)),
                Err(Unbound::NoAccount) => "no account".to_owned(),
                Err(Unbound::NotAllowed { .. }) => "account not allowed".to_owned(),
            };
            let account = resolved.ok();
            let models = self.0.models(id).await.map_err(hub_error)?;
            providers.push(ProviderModels {
                id: id.to_owned(),
                models: models
                    .into_iter()
                    .map(|model| (model.id, model.name))
                    .collect(),
                background,
                account,
            });
        }
        Ok(providers)
    }

    async fn start(
        &self,
        parent: ThreadId,
        turn: Option<u64>,
        child: NewChild,
    ) -> Result<Started, ThreadError> {
        let NewChild {
            task,
            title,
            provider,
            model,
            client_request_id,
        } = child;
        let task = ChildTask {
            task,
            title,
            provider,
            model,
            client_request_id,
        };
        let started = self
            .0
            .start_child(parent, turn, task)
            .await
            .map_err(hub_error)?;
        let status = match self.0.assistant(started.row.id).await {
            Ok(Some(assistant)) => assistant.status(),
            Ok(None) => return Err(ThreadError::NoSuchThread),
            Err(error) => return Err(hub_error(error)),
        };
        Ok(Started {
            thread: started.row.id,
            title: started.row.title,
            status,
            joined: started.joined,
            seq: started.seq,
        })
    }

    async fn send(
        &self,
        caller: ThreadId,
        turn: Option<u64>,
        thread: ThreadId,
        text: String,
    ) -> Result<u64, ThreadError> {
        self.0
            .send_child(caller, turn, thread, text)
            .await
            .map_err(hub_error)
    }

    async fn read(
        &self,
        caller: ThreadId,
        thread: ThreadId,
        after: u64,
    ) -> Result<ReadThread, ThreadError> {
        let (row, status, entries) = self
            .0
            .read_child(caller, thread, after)
            .await
            .map_err(hub_error)?;
        Ok(ReadThread {
            title: row.title,
            status,
            entries,
        })
    }

    async fn children(&self, caller: ThreadId) -> Result<(Vec<Child>, usize), ThreadError> {
        let (children, more) = self.0.children(caller).await.map_err(hub_error)?;
        let children = children
            .into_iter()
            .map(|(row, summary)| Child {
                thread: row.id,
                title: row.title,
                parent: row.parent,
                status: summary.status,
                provider: row.provider,
                model: row.model,
                archived: row.archived_at.is_some(),
                open_requests: summary.open_requests,
            })
            .collect();
        Ok((children, more))
    }

    async fn cancel(
        &self,
        caller: ThreadId,
        thread: ThreadId,
        reason: Option<String>,
    ) -> Result<bool, ThreadError> {
        self.0
            .cancel_child(caller, thread, reason.as_deref())
            .await
            .map_err(hub_error)
    }

    async fn wait(
        &self,
        caller: ThreadId,
        caller_turn: Option<u64>,
        thread: ThreadId,
        seq: u64,
        timeout: Duration,
        cancelled: CancellationToken,
    ) -> Result<Waited, ThreadError> {
        let end = self
            .0
            .wait_child(caller, caller_turn, thread, seq, timeout, &cancelled)
            .await
            .map_err(hub_error)?;
        Ok(match end {
            ChildEnd::Ended { outcome, answer } => Waited::Ended { outcome, answer },
            ChildEnd::WaitingForUser => Waited::WaitingForUser,
            ChildEnd::Queued => Waited::Queued,
            ChildEnd::Woken => Waited::Woken,
            ChildEnd::TimedOut => Waited::TimedOut,
            ChildEnd::GaveUp => Waited::GaveUp,
        })
    }

    async fn ask(&self, caller: ThreadId, ask: Ask) -> Result<Option<Waiter>, ThreadError> {
        self.0.ask_during(caller, ask).await.map_err(hub_error)
    }

    async fn is_sub_agent(&self, thread: ThreadId) -> bool {
        match self.0.get(thread).await {
            Ok(Some(row)) => row.parent.is_some(),
            Ok(None) => true,
            Err(error) => {
                tracing::error!("cannot read thread {thread}: {error:#}");
                true
            }
        }
    }
}

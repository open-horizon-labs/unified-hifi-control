//! Lifecycle-owned command endpoints for URI library providers. Roon/LMS use their existing
//! provider endpoints; URI providers share this worker rather than growing surface-owned tasks.
use super::{LibraryAdapter, Startable};
use crate::bus::runtime::{CommandEndpoint, CommandGateway, NativeResult, RuntimeCommand};
use crate::library_playback::DurableTarget;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

pub struct LibraryCommandWorker {
    name: &'static str,
    provider: &'static str,
    adapter: Arc<dyn LibraryAdapter>,
    gateway: CommandGateway,
    task: Mutex<Option<(CancellationToken, JoinHandle<()>)>>,
    transition: Semaphore,
    shutdown: CancellationToken,
}

impl LibraryCommandWorker {
    pub fn new(
        name: &'static str,
        provider: &'static str,
        adapter: Arc<dyn LibraryAdapter>,
        gateway: CommandGateway,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            name,
            provider,
            adapter,
            gateway,
            task: Mutex::new(None),
            transition: Semaphore::new(1),
            shutdown,
        }
    }
}

impl Drop for LibraryCommandWorker {
    fn drop(&mut self) {
        if let Some((shutdown, task)) = self.task.get_mut().take() {
            shutdown.cancel();
            task.abort();
        }
    }
}

#[async_trait]
impl Startable for LibraryCommandWorker {
    fn name(&self) -> &'static str {
        self.name
    }
    async fn start(&self) -> Result<()> {
        let _transition = self
            .transition
            .acquire()
            .await
            .map_err(|_| anyhow!("library lifecycle admission is closed"))?;
        anyhow::ensure!(!self.shutdown.is_cancelled(), "server is shutting down");
        if self.task.lock().await.is_some() {
            return Ok(());
        }
        let endpoint = self
            .gateway
            .register_provider(self.provider, 16)
            .map_err(|error| anyhow!("library endpoint registration failed: {error:?}"))?;
        let shutdown = self.shutdown.child_token();
        let run_shutdown = shutdown.clone();
        let adapter = self.adapter.clone();
        let provider = self.provider;
        let join = tokio::spawn(async move {
            run(adapter, provider, endpoint, run_shutdown).await;
        });
        *self.task.lock().await = Some((shutdown, join));
        Ok(())
    }
    async fn stop(&self) {
        let Ok(_transition) = self.transition.acquire().await else {
            return;
        };
        let running = { self.task.lock().await.take() };
        if let Some((shutdown, join)) = running {
            shutdown.cancel();
            if let Err(error) = join.await {
                tracing::warn!(%error, "library endpoint stopped");
            }
        }
    }
}

async fn run(
    adapter: Arc<dyn LibraryAdapter>,
    provider: &str,
    mut endpoint: CommandEndpoint,
    shutdown: CancellationToken,
) {
    loop {
        let work = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            work = endpoint.recv() => work,
        };
        let Some(work) = work else { break };
        let Ok(permit) = work.begin_dispatch() else {
            continue;
        };
        let zone = permit.request().target.to_string();
        let command = permit.request().command.clone();
        let native = async {
            let RuntimeCommand::LibraryPlay(request) = command else {
                return Err(anyhow!(
                    "non-library command routed to URI library endpoint"
                ));
            };
            let uri = match request.target {
                DurableTarget::Spotify { uri } if provider == "spotify" => uri,
                DurableTarget::MusicAssistant { uri } if provider == "musicassistant" => uri,
                _ => return Err(anyhow!("item provider does not match library endpoint")),
            };
            adapter.play_uri(&zone, &uri).await.map(|_| ())
        };
        match permit.run_native(&shutdown, native).await {
            crate::bus::runtime::NativeAttempt::Completed(Ok(())) => {
                permit.complete_native(NativeResult::Accepted)
            }
            crate::bus::runtime::NativeAttempt::Completed(Err(error)) => {
                permit.complete_native(NativeResult::Failed(error.to_string()))
            }
            crate::bus::runtime::NativeAttempt::Cancelled => break,
            crate::bus::runtime::NativeAttempt::TimedOut => {}
        }
    }
}

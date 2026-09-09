//! Engine-owned discovery/proxy lifecycle, independent of headed UI lifetimes.
use crate::{
    catalog::Catalog,
    discovery,
    mux::{self, BoxIo, Connector},
    peer::Peers,
    proxy, signaling,
};
use futures::{StreamExt, stream};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
#[derive(Clone)]
pub struct PreviewService(Arc<Inner>);
struct Inner {
    catalog: Catalog,
    stop: CancellationToken,
    started: AtomicBool,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}
pub type Projects = Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>;
impl PreviewService {
    pub fn new(file: PathBuf, device_id: String, device_name: String) -> anyhow::Result<Self> {
        Ok(Self(Arc::new(Inner {
            catalog: Catalog::open(file, device_id, device_name)?,
            stop: CancellationToken::new(),
            started: AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
        })))
    }
    pub fn catalog(&self) -> &Catalog {
        &self.0.catalog
    }
    pub async fn start(&self, projects: Projects, signaling: Option<signaling::Config>) {
        if self.0.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let connector = Arc::new(LocalConnector(self.0.catalog.clone()));
        let local = mux::local(connector.clone(), self.0.stop.child_token());
        let (peers, output) = Peers::new(
            self.0.catalog.device_id().into(),
            connector,
            self.0.stop.child_token(),
        );
        let router = proxy::Router {
            catalog: self.0.catalog.clone(),
            local,
            peers: peers.clone(),
        };
        let proxy_catalog = self.0.catalog.clone();
        let proxy_stop = self.0.stop.clone();
        let listener_task = tokio::spawn(async move {
            loop {
                let binding = tokio::select! {
                    _ = proxy_stop.cancelled() => break,
                    binding = proxy::serve(router.clone(), zeron_proto::PREVIEW_PROXY_PORT, proxy_stop.child_token()) => binding,
                };
                match binding {
                    Ok((port, tasks)) => {
                        proxy_catalog.set_proxy_status(port, None);
                        for task in tasks {
                            let _ = task.await;
                        }
                        break;
                    }
                    Err(error) => proxy_catalog.set_proxy_status(
                        zeron_proto::PREVIEW_PROXY_PORT,
                        Some(format!(
                            "Local previews could not listen on port 7331: {error}"
                        )),
                    ),
                }
                tokio::select! { _ = proxy_stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(2)) => {} }
            }
        });
        self.0.tasks.lock().unwrap().push(listener_task);
        let catalog = self.0.catalog.clone();
        let stop = self.0.stop.clone();
        let scanner = tokio::spawn(async move {
            loop {
                let scan = async {
                    let projects = projects.clone();
                    let candidates = tokio::task::spawn_blocking(move || {
                        let mut roots: Vec<_> = projects()
                            .into_iter()
                            .collect::<std::collections::BTreeSet<_>>()
                            .into_iter()
                            .filter_map(|p| p.canonicalize().ok())
                            .collect::<std::collections::BTreeSet<_>>()
                            .into_iter()
                            .collect();
                        if roots.is_empty() {
                            return Vec::new();
                        }
                        roots.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
                        discovery::listeners()
                            .into_iter()
                            .filter(|l| {
                                l.address.port() != zeron_proto::PREVIEW_PROXY_PORT
                                    && l.pid != std::process::id()
                            })
                            .filter_map(|listener| {
                                roots
                                    .iter()
                                    .find(|root| listener.belongs_to(root))
                                    .cloned()
                                    .map(|root| (root, listener))
                            })
                            .collect::<Vec<_>>()
                    })
                    .await
                    .unwrap_or_default();
                    let servers = stream::iter(candidates)
                        .map(|(root, listener)| async move {
                            discovery::is_http(listener.address)
                                .await
                                .then_some((root, listener))
                        })
                        .buffer_unordered(12)
                        .filter_map(|item| async { item })
                        .collect()
                        .await;
                    if let Err(error) = catalog.replace_local(servers) {
                        tracing::warn!(%error, "could not update preview services");
                    }
                };
                tokio::select! { _ = stop.cancelled() => break, _ = scan => {} }
                tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(2)) => {} }
            }
        });
        let mut tasks = self.0.tasks.lock().unwrap();
        tasks.push(scanner);
        if let Some(config) = signaling {
            tasks.push(tokio::spawn(signaling::run(
                config,
                self.0.catalog.clone(),
                peers,
                output,
                self.0.stop.child_token(),
            )));
        }
    }
    pub fn stop(&self) {
        self.0.stop.cancel();
        self.0.catalog.clear_remote();
    }
    pub async fn shutdown(&self) {
        self.stop();
        let tasks = std::mem::take(&mut *self.0.tasks.lock().unwrap());
        for task in tasks {
            let _ = task.await;
        }
    }
}
struct LocalConnector(Catalog);
#[async_trait::async_trait]
impl Connector for LocalConnector {
    async fn connect(&self, id: &str) -> anyhow::Result<BoxIo> {
        let route = self
            .0
            .local_route(id)
            .ok_or_else(|| anyhow::anyhow!("preview service stopped"))?;
        // Recheck process ownership before dialing: a stopped dev server's port
        // may have been reused by an unrelated process since the last scan.
        let expected = route.listener.clone();
        let valid = tokio::task::spawn_blocking(move || {
            discovery::listeners().iter().any(|actual| {
                actual.pid == expected.pid
                    && actual.started_at == expected.started_at
                    && actual.cwd == expected.cwd
                    && actual.address == expected.address
            })
        })
        .await?;
        anyhow::ensure!(valid, "preview process changed; waiting for rediscovery");
        Ok(Box::new(
            tokio::net::TcpStream::connect(route.listener.address).await?,
        ))
    }
}

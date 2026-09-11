//! The UI's real engine handle, independent of engine bootstrap and native I/O.
//! A connected browser transport must pass identity/readiness before attachment.
//! This module does not own application watches, drafts, or mutation policy.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::watch;
use zeron_proto::EngineInfo;
use zeron_rpc::{RpcClient, RpcError, methods};

/// How this UI reached its engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineMode {
    InProcess,
    Remote { url: String },
}

#[async_trait]
pub(super) trait EngineBackend: Send + Sync {
    fn client(&self) -> &RpcClient;
    fn mode(&self) -> EngineMode;
    async fn shutdown(&self);
}

#[derive(Clone)]
pub(super) enum DeferredEngineState {
    Waiting,
    Ready,
    Failed(String),
}

/// Shared by native bootstrap and authenticated browser attachment. The
/// concrete typed client and protocol reducers are identical on both targets.
#[derive(Clone)]
pub struct EngineHandle {
    pub(super) inner: Arc<dyn EngineBackend>,
    pub(super) engine_info: EngineInfo,
    pub(super) deferred_state: Option<watch::Receiver<DeferredEngineState>>,
}

impl EngineHandle {
    /// Attach an already authenticated transport without probing localhost or
    /// embedding an engine. The caller owns timeout/reconnect/auth epochs and
    /// must discard a completed attachment when its authentication epoch changes.
    /// No application watch or mutation is started by this handshake.
    pub async fn from_connected_client(client: RpcClient, url: String) -> Result<Self, RpcError> {
        let engine_info = client
            .call_as(methods::ENGINE_INFO, serde_json::json!({}))
            .await?;
        #[derive(serde::Deserialize)]
        struct Ready {
            ready: bool,
        }
        let ready: Ready = client
            .call_as(methods::ENGINE_READY, serde_json::json!({}))
            .await?;
        if !ready.ready {
            return Err(RpcError::Failed("Engine is not ready".into()));
        }
        Ok(Self {
            inner: Arc::new(ConnectedEngine { client, url }),
            engine_info,
            deferred_state: None,
        })
    }

    pub fn client(&self) -> &RpcClient {
        self.inner.client()
    }

    pub fn mode(&self) -> EngineMode {
        self.inner.mode()
    }

    pub fn engine_info(&self) -> &EngineInfo {
        &self.engine_info
    }

    pub(super) fn deferred_state(&self) -> Option<watch::Receiver<DeferredEngineState>> {
        self.deferred_state.clone()
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

struct ConnectedEngine {
    client: RpcClient,
    url: String,
}

#[async_trait]
impl EngineBackend for ConnectedEngine {
    fn client(&self) -> &RpcClient {
        &self.client
    }

    fn mode(&self) -> EngineMode {
        EngineMode::Remote {
            url: self.url.clone(),
        }
    }

    async fn shutdown(&self) {
        // Close only this viewport's transport; never send StopEngine.
        self.client.close();
    }
}

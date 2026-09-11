//! Executor boundary only; protocol state and backpressure remain in RpcClient.

#[cfg(not(target_arch = "wasm32"))]
pub(super) use tokio::spawn;
#[cfg(not(target_arch = "wasm32"))]
pub(super) type Task = tokio::task::JoinHandle<()>;

#[cfg(target_arch = "wasm32")]
pub(super) struct Task(futures::future::AbortHandle);

#[cfg(target_arch = "wasm32")]
impl Task {
    pub(super) fn abort(&self) {
        self.0.abort();
    }
}

#[cfg(target_arch = "wasm32")]
pub(super) fn spawn(future: impl Future<Output = ()> + 'static) -> Task {
    let (handle, registration) = futures::future::AbortHandle::new_pair();
    wasm_bindgen_futures::spawn_local(async move {
        let _ = futures::future::Abortable::new(future, registration).await;
    });
    Task(handle)
}

//! Real OS process/listener association, including unrelated project isolation.
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use zeron_preview::PreviewService;
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn launch(cwd: &std::path::Path, http: bool) -> Child {
    let mut command = std::process::Command::new("python3");
    if http {
        // HTTPServer.server_bind does a reverse DNS lookup before listen(),
        // which can stall on isolated macOS runners. TCPServer exercises the
        // same real HTTP handler without depending on external DNS readiness.
        command.args(["-c", "import http.server,socketserver; socketserver.TCPServer(('127.0.0.1',0),http.server.SimpleHTTPRequestHandler).serve_forever()"]);
    } else {
        command.args(["-c", "import socket,time; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(); time.sleep(30)"]);
    }
    Child(
        command
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}
async fn wait(service: &PreviewService, expected_pid: Option<u32>) {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let services = service.catalog().snapshot().services;
            if match expected_pid {
                Some(pid) => services.len() == 1 && services[0].pid == pid,
                None => services.is_empty(),
            } {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("discovery did not converge");
}
#[tokio::test]
async fn only_current_project_http_processes_are_exposed_and_removals_are_live() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("app");
    let b = temp.path().join("unrelated");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    let local = launch(&a, true);
    let other = launch(&b, true);
    let _non_http = launch(&a, false);
    let roots = Arc::new(Mutex::new(vec![a.clone()]));
    let projects = roots.clone();
    let service = PreviewService::new(
        temp.path().join("names.json"),
        "local".into(),
        "Laptop".into(),
    )
    .unwrap();
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:7331").await.ok();
    service
        .start(Arc::new(move || projects.lock().unwrap().clone()), None)
        .await;
    wait(&service, Some(local.0.id())).await;
    if let Some(listener) = occupied {
        assert!(service.catalog().snapshot().error.is_some());
        drop(listener);
        tokio::time::timeout(Duration::from_secs(6), async {
            while service.catalog().snapshot().error.is_some() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("proxy did not retry the freed port");
    }
    let found = &service.catalog().snapshot().services[0];
    assert!(found.zeron_owned);
    assert_eq!(std::path::Path::new(&found.cwd), a.canonicalize().unwrap());
    assert!(found.started_at > 0);
    *roots.lock().unwrap() = vec![b];
    wait(&service, Some(other.0.id())).await;
    drop(other);
    wait(&service, None).await;
    service.shutdown().await;
    // Shutdown fully releases the fixed proxy port before a profile boots.
    if service.catalog().snapshot().error.is_none() {
        assert!(
            tokio::net::TcpListener::bind("127.0.0.1:7331")
                .await
                .is_ok()
        );
    }
}

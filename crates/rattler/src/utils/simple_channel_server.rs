use axum::body::{Empty, StreamBody};
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{body, Extension};
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::sync::oneshot;
use tokio_util::io::ReaderStream;
use url::Url;

pub struct SimpleChannelServer {
    local_addr: SocketAddr,
    shutdown_sender: Option<oneshot::Sender<()>>,
}

impl SimpleChannelServer {
    /// Returns the root Url to the server
    pub fn url(&self) -> Url {
        Url::parse(&format!("http://localhost:{}", self.local_addr.port())).unwrap()
    }
}

async fn static_path(
    Path(path): Path<String>,
    Extension(root): Extension<PathBuf>,
) -> impl IntoResponse {
    let path = root.join(path.trim_start_matches('/'));
    match tokio::fs::OpenOptions::default()
        .read(true)
        .write(false)
        .open(path)
        .await
    {
        Ok(file) => Response::builder()
            .status(StatusCode::OK)
            .body(body::boxed(StreamBody::new(ReaderStream::new(file))))
            .unwrap(),
        Err(_) => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(body::boxed(Empty::new()))
            .unwrap(),
    }
}

impl SimpleChannelServer {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let app = axum::Router::new()
            .route("/*path", get(static_path))
            .layer(Extension(path.into()));

        // Construct the server
        let addr = SocketAddr::new([127, 0, 0, 1].into(), 0);
        let server = axum::Server::bind(&addr).serve(app.into_make_service());

        // Get the address of the server
        let addr = server.local_addr();

        // Setup a graceful shutdown trigger which is fired when this instance is dropped.
        let (tx, rx) = oneshot::channel();
        let server = server.with_graceful_shutdown(async {
            rx.await.ok();
        });

        // Spawn the server. Let go of the JoinHandle, we can use the graceful shutdown trigger to
        // stop the server.
        let _ = tokio::spawn(server);

        Self {
            local_addr: addr,
            shutdown_sender: Some(tx),
        }
    }
}

impl Drop for SimpleChannelServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_sender.take() {
            let _ = tx.send(());
        }
    }
}

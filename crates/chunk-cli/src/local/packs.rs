//! Serves the resource packs of the releases dev staged to its players' clients, over plain HTTP/1.1.

use std::{
    collections::BTreeSet,
    convert::Infallible,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use http_body_util::Full;
use hyper::{Method, Request, Response, StatusCode, body::Bytes, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::{net::TcpListener, task::JoinSet};

/// The pack server, which stops when this drops.
pub(super) struct Packs {
    url_prefix: String,
    allowed: Arc<Mutex<BTreeSet<String>>>,
    server: tokio::task::AbortHandle,
}

impl Packs {
    /// Starts serving packs from the asset store at `store` on `bind`, to clients that reach it at `public`, or else
    /// at the address it listens on.
    pub async fn start(store: PathBuf, bind: SocketAddr, public: Option<&url::Url>) -> io::Result<Self> {
        let listener = TcpListener::bind(bind)
            .await
            .map_err(|error| io::Error::new(error.kind(), format!("cannot serve packs on {bind}: {error}")))?;
        let url_prefix = url_prefix(listener.local_addr()?, public)?;
        let allowed = Arc::default();
        let server = tokio::spawn(serve(listener, store.join("blobs"), Arc::clone(&allowed))).abort_handle();
        Ok(Self { url_prefix, allowed, server })
    }

    /// Where clients download a pack: this prefix followed by its SHA-256.
    pub fn url_prefix(&self) -> &str {
        &self.url_prefix
    }

    /// Serves the packs of a staged release's asset revision.
    pub fn serve(&self, assets: &chunk_control::DeploymentAssets) {
        lock(&self.allowed).extend(assets.packs.values().map(|pack| pack.sha256.clone()));
    }
}

impl Drop for Packs {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Where clients download packs: `public`'s path followed by `packs/`, or else the listening address's.
pub(super) fn url_prefix(listening: SocketAddr, public: Option<&url::Url>) -> io::Result<String> {
    let Some(public) = public else {
        if listening.ip().is_unspecified() {
            return Err(io::Error::other(format!(
                "--pack-bind {listening} listens on every address; set --pack-url to the one players' clients reach"
            )));
        }
        return Ok(format!("http://{listening}/packs/"));
    };
    if !["http", "https"].contains(&public.scheme()) || public.host().is_none() {
        return Err(io::Error::other(format!("--pack-url {public} must be an http or https URL with a host")));
    }
    if public.query().is_some() || public.fragment().is_some() {
        return Err(io::Error::other(format!("--pack-url {public} cannot have a query or fragment")));
    }
    Ok(format!("{}/packs/", public.as_str().trim_end_matches('/')))
}

async fn serve(listener: TcpListener, blobs: PathBuf, allowed: Arc<Mutex<BTreeSet<String>>>) {
    let mut connections = JoinSet::new();
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::warn!(%error, "pack server accept failed; retrying");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        while connections.try_join_next().is_some() {}
        let (blobs, allowed) = (blobs.clone(), allowed.clone());
        let service = service_fn(move |request: Request<_>| {
            let pack = request.uri().path().strip_prefix("/packs/").map(str::to_owned);
            let pack = pack.filter(|pack| request.method() == Method::GET && lock(&allowed).contains(pack));
            let path = pack.map(|pack| blobs.join(pack));
            async move { Ok::<_, Infallible>(respond(path).await) }
        });
        let connection = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service);
        connections.spawn(async move { _ = connection.await });
    }
}

async fn respond(path: Option<PathBuf>) -> Response<Full<Bytes>> {
    let body = match path {
        Some(path) => tokio::fs::read(path).await.ok(),
        None => None,
    };
    let Some(body) = body else {
        let mut response = Response::new(Full::from("not found\n"));
        *response.status_mut() = StatusCode::NOT_FOUND;
        return response;
    };
    Response::new(Full::from(body))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;

use std::time::Duration;

use tonic::{Request, transport::Channel};

use crate::{Error, Result, RuntimeConnection};

pub(crate) async fn channel(runtime: &RuntimeConnection) -> Result<Channel> {
    let endpoint = runtime.endpoint.strip_prefix("http://").ok_or(Error::Invalid("local runtime URL"))?;
    let address: std::net::SocketAddr = endpoint.parse().map_err(|_| Error::Invalid("runtime address"))?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(Error::Invalid("runtime must be loopback"));
    }
    Channel::from_shared(runtime.endpoint.clone())
        .map_err(|_| Error::Invalid("runtime URL"))?
        .connect_timeout(Duration::from_secs(3))
        .connect()
        .await
        .map_err(|_| Error::Unresolved("runtime connection unavailable"))
}

pub(crate) fn auth<T>(runtime: &RuntimeConnection, body: T, seconds: u64) -> Result<Request<T>> {
    let mut request = Request::new(body);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", runtime.token).parse().map_err(|_| Error::Invalid("runtime credential"))?,
    );
    request.set_timeout(Duration::from_secs(seconds));
    Ok(request)
}

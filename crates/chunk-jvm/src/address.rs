//! The address players reach the JVM at, when the machine's environment names none.

use crate::{
    Failure,
    config::Config,
    fetch::{Attempt, retry},
};
use std::{net::IpAddr, time::Duration};
use tokio::{net::TcpStream, time::timeout};

/// The local address of a connection to core, which is the address core sees this machine at. It must be private.
pub(crate) async fn detect(config: &Config) -> Result<IpAddr, Failure> {
    let address = retry(config.retry, || async {
        let connected = timeout(Duration::from_secs(5), TcpStream::connect(config.core)).await;
        match connected {
            Ok(Ok(stream)) => {
                stream.local_addr().map(|local| local.ip().to_canonical()).map_err(|error| Failure::io(error).into())
            }
            Ok(Err(error)) => Err(Attempt::Transient(format!("cannot connect to core: {error}"))),
            Err(_) => Err(Attempt::Transient("connecting to core timed out".into())),
        }
    })
    .await?;
    if !chunk_service::net::private(address) {
        return Err(Failure::env(format!(
            "this machine reaches core from {address}, which is not private; set CHUNK_PLAYER_ADDRESS"
        )));
    }
    Ok(address)
}

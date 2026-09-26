use std::{io, sync::Arc};

use anyhow::{Result, ensure};
use chunk_proxy::benchmark::{Peer, relay};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, Payload};

pub fn payload(length: usize, pattern: Payload, seed: u64) -> Vec<u8> {
    let mut body = vec![0; length];
    let mut state = seed.max(1);
    for (index, byte) in body.iter_mut().enumerate().skip(9) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = match pattern {
            Payload::Repeated => 42,
            Payload::Mixed if index % 2 == 0 => 42,
            Payload::Mixed | Payload::Random => state.to_le_bytes()[0],
        };
    }
    // Opaque PLAY packet; avoids settings and command-tree packet IDs.
    body[0] = 0x7f;
    body
}

pub struct Client {
    peer: Peer,
    request: Vec<u8>,
    response: Vec<u8>,
    burst: usize,
}

impl Client {
    pub async fn connect(endpoint: &str, config: &Config) -> Result<Self> {
        let stream = TcpStream::connect(endpoint).await?;
        stream.set_nodelay(true)?;
        Ok(Self {
            peer: Peer::new(stream, !config.no_encryption, config.compression())?,
            request: payload(config.request_bytes, config.payload, config.seed),
            response: payload(config.response_bytes, config.payload, config.seed),
            burst: config.burst,
        })
    }

    pub async fn exchange(&mut self, sequence: u64) -> Result<()> {
        self.request[1..9].copy_from_slice(&sequence.to_le_bytes());
        self.response[1..9].copy_from_slice(&sequence.to_le_bytes());
        self.peer.write(&self.request).await?;
        for _ in 0..self.burst {
            ensure!(self.peer.read().await?.as_ref() == self.response, "relay response payload mismatch");
        }
        Ok(())
    }
}

pub async fn gameplay(listener: TcpListener, config: &Config, stop: CancellationToken) -> Result<()> {
    let request = Arc::new(payload(config.request_bytes, config.payload, config.seed));
    let response = Arc::new(payload(config.response_bytes, config.payload, config.seed));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            result = listener.accept() => {
                let (stream, _) = result?;
                stream.set_nodelay(true)?;
                let request = request.clone();
                let mut response = response.as_ref().clone();
                let burst = config.burst;
                tasks.spawn(async move {
                    let mut peer = Peer::new(stream, false, None)?;
                    loop {
                        let received = peer.read().await?;
                        if received.len() != request.len() || received[0] != 0x7f || received[9..] != request[9..] {
                            return Err(io::Error::other("relay request payload mismatch"));
                        }
                        response[1..9].copy_from_slice(&received[1..9]);
                        peer.write_burst(&response, burst).await?;
                    }
                });
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                result?.or_else(closed)?;
            }
        }
    }
    tasks.shutdown().await;
    Ok(())
}

pub async fn target(listener: TcpListener, backend: &str, config: &Config, stop: CancellationToken) -> Result<()> {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            result = listener.accept() => {
                let (stream, _) = result?;
                // Matches the production player listener.
                stream.set_nodelay(true)?;
                let backend = backend.to_owned();
                let encrypted = !config.no_encryption;
                let compression = config.compression();
                let stop = stop.clone();
                tasks.spawn(async move {
                    let internal = TcpStream::connect(backend).await?;
                    internal.set_nodelay(true)?;
                    relay(Peer::new(stream, encrypted, compression)?, Peer::new(internal, false, None)?, stop.cancelled()).await
                });
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                result?.or_else(closed)?;
            }
        }
    }
    tasks.shutdown().await;
    Ok(())
}

fn closed(error: io::Error) -> io::Result<()> {
    match error.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe => Ok(()),
        _ => Err(error),
    }
}

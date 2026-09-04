use std::{
    future::{Future, Ready, ready},
    io,
    net::SocketAddr,
};

use crate::Config;

/// No proxy can be constructed without an enabled protocol version.
pub enum Proxy {}

impl Proxy {
    /// # Errors
    /// Always returns a configuration error because no version is enabled.
    pub fn bind(_: SocketAddr, _: Config) -> Ready<io::Result<Self>> {
        ready(Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no Minecraft version enabled; enable a version feature",
        )))
    }

    /// # Errors
    /// Unreachable: a proxy cannot be constructed without a version.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match *self {}
    }

    /// # Errors
    /// Unreachable: a proxy cannot be constructed without a version.
    pub fn run(self, _: impl Future<Output = io::Result<()>>) -> Ready<io::Result<()>> {
        match self {}
    }
}

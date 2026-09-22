use std::{
    future::{Future, Ready, ready},
    io,
    net::SocketAddr,
};

use crate::{Config, PlatformTarget};

/// No proxy can be constructed without an enabled protocol version.
pub enum Proxy {}

/// Unconstructible without an enabled protocol version.
#[derive(Clone)]
pub enum Retarget {}

impl Retarget {
    /// # Errors
    /// Unreachable: a proxy cannot be constructed without a version.
    pub fn replace(&self, _: PlatformTarget) -> io::Result<()> {
        match *self {}
    }
}

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

    #[must_use]
    pub fn retarget(&self) -> Option<Retarget> {
        match *self {}
    }

    /// # Errors
    /// Unreachable: a proxy cannot be constructed without a version.
    pub fn run(self, _: impl Future<Output = io::Result<()>>) -> Ready<io::Result<()>> {
        match self {}
    }
}

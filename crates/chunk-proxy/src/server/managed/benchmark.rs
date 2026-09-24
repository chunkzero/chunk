//! Established-connection adapters for `chunk-bench`. No listener or login policy is changed.

use std::{future::Future, io};

use chunk_protocol::{
    McString, VarInt,
    versions::v26_2::{ConfigurationClientInformation, ConfigurationClientInformationParticleStatus},
};
use tokio::net::TcpStream;

use super::super::transport::Transport;

/// A benchmark peer using the production frame, compression and encryption implementation.
pub struct Peer(Transport<TcpStream>);

impl Peer {
    /// # Errors
    /// Returns encryption initialization errors.
    pub fn new(stream: TcpStream, encrypted: bool, compression: Option<usize>) -> io::Result<Self> {
        let mut transport = Transport::new(stream);
        if encrypted {
            transport.enable_encryption(&[42; 16])?;
        }
        if let Some(threshold) = compression {
            transport.enable_compression(threshold);
        }
        Ok(Self(transport))
    }

    /// # Errors
    /// Returns packet encoding or socket errors.
    pub async fn write(&mut self, body: &[u8]) -> io::Result<()> {
        self.0.write_body(body).await
    }

    /// # Errors
    /// Returns frame validation or socket errors.
    pub async fn read(&mut self) -> io::Result<bytes::Bytes> {
        self.0.read_frame(chunk_protocol::MAX_FRAME_SIZE).await
    }
}

/// Runs the managed PLAY packet pump after synthetic negotiation.
/// Commands, admission, moves and external authentication are outside this measurement.
/// # Errors
/// Returns transport or packet errors.
pub async fn relay(mut public: Peer, mut internal: Peer, stop: impl Future<Output = ()>) -> io::Result<()> {
    let mut settings = ConfigurationClientInformation {
        locale: McString::new("en_US").map_err(io::Error::other)?,
        view_distance: 10,
        chat_flags: VarInt(0),
        chat_colors: true,
        skin_parts: 127,
        main_hand: VarInt(1),
        enable_text_filtering: false,
        enable_server_listing: true,
        particle_status: ConfigurationClientInformationParticleStatus::Minimal,
    };
    super::relay::until(&mut public.0, &mut internal.0, &mut settings, stop, true, None).await
}

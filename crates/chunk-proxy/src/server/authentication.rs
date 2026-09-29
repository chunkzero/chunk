use std::{fmt::Write as _, io, time::Duration};

use chunk_protocol::{
    BoundedArray, ByteArray, McString, Uuid, VarInt, decode_packet,
    versions::v26_2::{
        EncryptionRequest, EncryptionResponse, LoginAcknowledged, LoginDisconnect, LoginStart, LoginSuccess,
        LoginSuccessPropertiesEntry, SetCompression,
    },
};
use openssl::{
    hash::{MessageDigest, hash},
    pkey::Private,
    rand::rand_bytes,
    rsa::{Padding, Rsa},
    sha::Sha1,
};
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite};
use zeroize::Zeroizing;

use super::transport::{Transport, invalid_data};

const SESSION_SERVER: &str = "https://sessionserver.mojang.com/session/minecraft/hasJoined";
const LOGIN_FRAME_LIMIT: usize = 4096;
const PROFILE_LIMIT: usize = 65536;

pub(super) struct Authentication {
    key: Rsa<Private>,
    public_key: Vec<u8>,
    client: Client,
    endpoint: Url,
    offline: bool,
}

/// A client that failed authentication: once sent the encryption request, it closed or reset the connection, as
/// offline-mode clients close it, or sent anything but a valid Encryption Response; or the session service didn't vouch
/// for the name it claimed.
#[derive(Debug)]
struct Unauthenticated(String);

impl std::fmt::Display for Unauthenticated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unauthenticated {}

fn unauthenticated(reason: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, Unauthenticated(reason.into()))
}

/// Whether `error` ended a login because the client failed authentication.
pub(super) fn failed(error: &io::Error) -> bool {
    error.get_ref().is_some_and(<dyn std::error::Error + Send + Sync>::is::<Unauthenticated>)
}

/// An authenticated identity and its connection, after Login Acknowledged.
pub(super) struct Authenticated<S> {
    pub protocol_version: i32,
    pub profile: LoginSuccess,
    pub transport: Transport<S>,
}

impl Authentication {
    /// With `offline`, logins skip encryption and Mojang verification, as vanilla offline mode does.
    pub(super) async fn new(offline: bool) -> io::Result<Self> {
        let key = tokio::task::spawn_blocking(|| Rsa::generate(1024)).await.map_err(io::Error::other)??;
        let public_key = key.public_key_to_der()?;
        let client = Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(io::Error::other)?;
        let endpoint = Url::parse(SESSION_SERVER).map_err(io::Error::other)?;
        Ok(Self { key, public_key, client, endpoint, offline })
    }

    pub(super) async fn login<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut transport: Transport<S>,
        protocol_version: i32,
        compression: Option<usize>,
    ) -> io::Result<Authenticated<S>> {
        let profile = self.negotiate(&mut transport, compression).await?;
        Ok(Authenticated { protocol_version, profile, transport })
    }

    async fn negotiate<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        transport: &mut Transport<S>,
        compression: Option<usize>,
    ) -> io::Result<LoginSuccess> {
        let start =
            decode_packet::<LoginStart>(&transport.read_frame(LOGIN_FRAME_LIMIT).await?).map_err(invalid_data)?;
        if !valid_username(start.username.as_str()) {
            return Err(invalid_data("invalid login username"));
        }
        let profile = if self.offline {
            offline_profile(start.username.as_str())?
        } else {
            self.authenticate(transport, start.username.as_str()).await?
        };
        if let Some(threshold) = compression {
            transport
                .write_packet(&SetCompression { threshold: VarInt(i32::try_from(threshold).map_err(invalid_data)?) })
                .await?;
            transport.enable_compression(threshold);
        }
        transport.write_packet(&profile).await?;
        decode_packet::<LoginAcknowledged>(&transport.read_frame(LOGIN_FRAME_LIMIT).await?).map_err(invalid_data)?;
        Ok(profile)
    }

    /// Encrypts the connection and verifies the session with Mojang.
    async fn authenticate<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        transport: &mut Transport<S>,
        username: &str,
    ) -> io::Result<LoginSuccess> {
        let mut token = [0; 4];
        rand_bytes(&mut token)?;
        transport
            .write_packet(&EncryptionRequest {
                server_id: McString::new("").map_err(invalid_data)?,
                public_key: ByteArray::new(self.public_key.clone()).map_err(invalid_data)?,
                verify_token: ByteArray::new(token.to_vec()).map_err(invalid_data)?,
                should_authenticate: true,
            })
            .await?;
        let secret = self
            .answer(transport, token)
            .await
            .map_err(|error| unauthenticated(format!("no valid answer to the encryption request: {error}")))?;
        transport.enable_encryption(&secret)?;
        let hash = server_hash(&secret, &self.public_key);
        match self.verify(username, &hash).await {
            Ok(profile) => Ok(profile),
            Err(error) => {
                let rejection = LoginDisconnect {
                    reason: McString::new(r#"{"text":"Unable to authenticate with Minecraft. Please try again."}"#)
                        .map_err(invalid_data)?,
                };
                let _ = transport.write_packet(&rejection).await;
                let _ = transport.shutdown().await;
                Err(error)
            }
        }
    }

    /// The shared secret from the client's Encryption Response. Every way this fails is the client's doing: closing,
    /// resetting, or sending anything but a valid response.
    async fn answer<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        transport: &mut Transport<S>,
        token: [u8; 4],
    ) -> io::Result<Zeroizing<[u8; 16]>> {
        let frame = transport.read_frame(LOGIN_FRAME_LIMIT).await?;
        self.shared_secret(&decode_packet::<EncryptionResponse>(&frame).map_err(invalid_data)?, token)
    }

    fn shared_secret(&self, response: &EncryptionResponse, token: [u8; 4]) -> io::Result<Zeroizing<[u8; 16]>> {
        let size = self.key.size() as usize;
        if response.shared_secret.as_slice().len() != size || response.verify_token.as_slice().len() != size {
            return Err(invalid_data("invalid encryption response"));
        }
        let mut secret = Zeroizing::new(vec![0; size]);
        let mut returned_token = vec![0; size];
        let secret_len = self.key.private_decrypt(response.shared_secret.as_slice(), &mut secret, Padding::PKCS1);
        let token_len = self.key.private_decrypt(response.verify_token.as_slice(), &mut returned_token, Padding::PKCS1);
        if secret_len.ok() != Some(16)
            || token_len.ok() != Some(4)
            || !openssl::memcmp::eq(&returned_token[..4], &token)
        {
            return Err(invalid_data("invalid encryption response"));
        }
        Ok(Zeroizing::new(secret[..16].try_into().expect("validated AES key length")))
    }

    async fn verify(&self, username: &str, hash: &str) -> io::Result<LoginSuccess> {
        let mut response = self
            .client
            .get(self.endpoint.clone())
            .query(&[("username", username), ("serverId", hash)])
            .send()
            .await
            .map_err(|_| io::Error::other("session service request failed"))?;
        // The session service answers a session it can't verify with No Content; other statuses are its own failures.
        if response.status() == StatusCode::NO_CONTENT {
            return Err(unauthenticated("session was not verified"));
        }
        if response.status() != StatusCode::OK {
            return Err(invalid_data("session service failed"));
        }
        if response.content_length().is_some_and(|length| length > PROFILE_LIMIT as u64) {
            return Err(invalid_data("session profile too large"));
        }
        let mut body = Vec::new();
        while let Some(chunk) =
            response.chunk().await.map_err(|_| io::Error::other("session service response failed"))?
        {
            if chunk.len() > PROFILE_LIMIT - body.len() {
                return Err(invalid_data("session profile too large"));
            }
            body.extend_from_slice(&chunk);
        }
        parse_profile(&body, username)
    }
}

/// The profile vanilla offline mode assigns: Java's `UUID.nameUUIDFromBytes("OfflinePlayer:" + name)` and no properties.
fn offline_profile(username: &str) -> io::Result<LoginSuccess> {
    let digest = hash(MessageDigest::md5(), format!("OfflinePlayer:{username}").as_bytes())?;
    let uuid = uuid::Builder::from_md5_bytes(digest.as_ref().try_into().expect("MD5 digest length")).into_uuid();
    Ok(LoginSuccess {
        session_id: Uuid(*uuid::Uuid::new_v4().as_bytes()),
        uuid: Uuid(uuid.into_bytes()),
        username: McString::new(username).map_err(invalid_data)?,
        properties: BoundedArray::new(Vec::new()).map_err(invalid_data)?,
    })
}

fn valid_username(name: &str) -> bool {
    !name.is_empty() && name.len() <= 16 && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[derive(Deserialize)]
struct SessionProfile {
    id: String,
    name: String,
    properties: Vec<Property>,
}

#[derive(Deserialize)]
struct Property {
    name: String,
    value: String,
    signature: Option<String>,
}

fn parse_profile(bytes: &[u8], username: &str) -> io::Result<LoginSuccess> {
    let profile: SessionProfile = serde_json::from_slice(bytes).map_err(|_| invalid_data("invalid session profile"))?;
    if !valid_username(&profile.name)
        || !profile.name.eq_ignore_ascii_case(username)
        || profile.id.len() != 32
        || !profile.id.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(unauthenticated("session profile identity mismatch"));
    }
    let mut uuid = [0; 16];
    for (output, pair) in uuid.iter_mut().zip(profile.id.as_bytes().as_chunks::<2>().0) {
        let hex = std::str::from_utf8(pair).map_err(|_| invalid_data("invalid profile UUID"))?;
        *output = u8::from_str_radix(hex, 16).map_err(|_| invalid_data("invalid profile UUID"))?;
    }
    let properties = profile
        .properties
        .into_iter()
        .map(|property| {
            Ok(LoginSuccessPropertiesEntry {
                name: McString::new(property.name)?,
                value: McString::new(property.value)?,
                signature: property.signature.map(McString::new).transpose()?,
            })
        })
        .collect::<chunk_protocol::Result<Vec<_>>>()
        .map_err(invalid_data)?;
    Ok(LoginSuccess {
        session_id: Uuid(*uuid::Uuid::new_v4().as_bytes()),
        uuid: Uuid(uuid),
        username: McString::new(profile.name).map_err(invalid_data)?,
        properties: BoundedArray::new(properties).map_err(invalid_data)?,
    })
}

fn server_hash(secret: &[u8; 16], public_key: &[u8]) -> String {
    let mut digest = Sha1::new();
    digest.update(secret);
    digest.update(public_key);
    signed_hex(digest.finish())
}

fn signed_hex(mut bytes: [u8; 20]) -> String {
    let negative = bytes[0] & 0x80 != 0;
    if negative {
        let mut carry = true;
        for byte in bytes.iter_mut().rev() {
            let (value, overflow) = (!*byte).overflowing_add(u8::from(carry));
            *byte = value;
            carry = overflow;
        }
    }
    let mut hex = String::new();
    for byte in bytes {
        write!(hex, "{byte:02x}").expect("write to string");
    }
    let magnitude = hex.trim_start_matches('0');
    if magnitude.is_empty() {
        "0".into()
    } else if negative {
        format!("-{magnitude}")
    } else {
        magnitude.into()
    }
}

#[cfg(test)]
mod tests;

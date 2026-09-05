use std::{fmt::Write as _, io, time::Duration};

use chunk_protocol::{
    BoundedArray, ByteArray, McString, Uuid, VarInt, decode_packet,
    versions::v26_1::{
        EncryptionRequest, EncryptionResponse, LoginAcknowledged, LoginDisconnect, LoginStart, LoginSuccess,
        LoginSuccessPropertiesEntry, SetCompression,
    },
};
use openssl::{
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
}

/// An authenticated identity and its connection, after Login Acknowledged.
pub(super) struct Authenticated<S> {
    pub protocol_version: i32,
    pub profile: LoginSuccess,
    pub transport: Transport<S>,
}

impl Authentication {
    pub(super) async fn new() -> io::Result<Self> {
        let key = tokio::task::spawn_blocking(|| Rsa::generate(1024))
            .await
            .map_err(io::Error::other)??;
        let public_key = key.public_key_to_der()?;
        let client = Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(io::Error::other)?;
        Ok(Self {
            key,
            public_key,
            client,
            endpoint: Url::parse(SESSION_SERVER).map_err(io::Error::other)?,
        })
    }

    pub(super) async fn login<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut transport: Transport<S>,
        protocol_version: i32,
        compression: Option<usize>,
    ) -> io::Result<Authenticated<S>> {
        let profile = self.negotiate(&mut transport, compression).await?;
        Ok(Authenticated {
            protocol_version,
            profile,
            transport,
        })
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
        let response = decode_packet::<EncryptionResponse>(&transport.read_frame(LOGIN_FRAME_LIMIT).await?)
            .map_err(invalid_data)?;
        let secret = self.shared_secret(&response, token)?;
        transport.enable_encryption(&secret)?;
        let hash = server_hash(&secret, &self.public_key);
        let profile = match self.verify(start.username.as_str(), &hash).await {
            Ok(profile) => profile,
            Err(error) => {
                let rejection = LoginDisconnect {
                    reason: McString::new(r#"{"text":"Unable to authenticate with Minecraft. Please try again."}"#)
                        .map_err(invalid_data)?,
                };
                let _ = transport.write_packet(&rejection).await;
                let _ = transport.shutdown().await;
                return Err(error);
            }
        };
        if let Some(threshold) = compression {
            transport
                .write_packet(&SetCompression {
                    threshold: VarInt(i32::try_from(threshold).map_err(invalid_data)?),
                })
                .await?;
            transport.enable_compression(threshold);
        }
        transport.write_packet(&profile).await?;
        decode_packet::<LoginAcknowledged>(&transport.read_frame(LOGIN_FRAME_LIMIT).await?).map_err(invalid_data)?;
        Ok(profile)
    }

    fn shared_secret(&self, response: &EncryptionResponse, token: [u8; 4]) -> io::Result<Zeroizing<[u8; 16]>> {
        let size = self.key.size() as usize;
        if response.shared_secret.as_slice().len() != size || response.verify_token.as_slice().len() != size {
            return Err(invalid_data("invalid encryption response"));
        }
        let mut secret = Zeroizing::new(vec![0; size]);
        let mut returned_token = vec![0; size];
        let secret_len = self
            .key
            .private_decrypt(response.shared_secret.as_slice(), &mut secret, Padding::PKCS1);
        let token_len = self
            .key
            .private_decrypt(response.verify_token.as_slice(), &mut returned_token, Padding::PKCS1);
        if secret_len.ok() != Some(16)
            || token_len.ok() != Some(4)
            || !openssl::memcmp::eq(&returned_token[..4], &token)
        {
            return Err(invalid_data("invalid encryption response"));
        }
        Ok(Zeroizing::new(
            secret[..16].try_into().expect("validated AES key length"),
        ))
    }

    async fn verify(&self, username: &str, hash: &str) -> io::Result<LoginSuccess> {
        let mut response = self
            .client
            .get(self.endpoint.clone())
            .query(&[("username", username), ("serverId", hash)])
            .send()
            .await
            .map_err(|_| io::Error::other("session service request failed"))?;
        if response.status() != StatusCode::OK {
            return Err(invalid_data("session was not verified"));
        }
        if response
            .content_length()
            .is_some_and(|length| length > PROFILE_LIMIT as u64)
        {
            return Err(invalid_data("session profile too large"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| io::Error::other("session service response failed"))?
        {
            if chunk.len() > PROFILE_LIMIT - body.len() {
                return Err(invalid_data("session profile too large"));
            }
            body.extend_from_slice(&chunk);
        }
        parse_profile(&body, username)
    }
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
        return Err(invalid_data("session profile identity mismatch"));
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
#[path = "authentication_tests.rs"]
mod tests;

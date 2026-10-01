//! One bot: an offline-mode client that logs in, answers what chunk's gateway and Minestom expect of a client, then
//! walks a small circle, pings and optionally runs a command until the run stops it.
use std::{
    net::SocketAddr,
    sync::{Arc, atomic::Ordering::Relaxed},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chunk_protocol::{
    BoundedArray, Decode, McString, Packet, Uuid, VarInt,
    versions::v26_2::{
        AcknowledgeConfiguration, ChunkBatchFinished, ChunkBatchReceived, ConfigurationAcknowledged,
        ConfigurationClientInformation, ConfigurationClientInformationParticleStatus, ConfigurationKeepAlive,
        ConfigurationKeepAliveResponse, ConfigurationPing, ConfigurationPong, ConfirmTeleport, EncryptionRequest,
        FinishConfiguration, Handshake, KnownPacks, LoginAcknowledged, LoginDisconnect, LoginStart, LoginSuccess,
        MovePosition, PlayKeepAlive, PlayKeepAliveResponse, PlayPing, PlayPong, PlayerLoaded, SelectKnownPacks,
        SetCompression, StartConfiguration, SynchronizePosition, UnsignedCommand, VERSION,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::OwnedSemaphorePermit,
    time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout_at},
};
use tokio_util::sync::CancellationToken;

use crate::{config::Config, packets, pings::Pings, stats::Stats, wire};

const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Radius of each bot's walking circle, in blocks, and its speed, a vanilla walk in blocks per second.
const RADIUS: f64 = 3.0;
const SPEED: f64 = 4.3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Login,
    Configuration,
    Play,
}

/// Packets whose payload the bot reads in each phase; the rest are skipped unread.
fn wanted(phase: Phase, id: i32) -> bool {
    match phase {
        Phase::Login => true,
        Phase::Configuration => matches!(
            id,
            ConfigurationKeepAlive::ID
                | ConfigurationPing::ID
                | SelectKnownPacks::ID
                | FinishConfiguration::ID
                | packets::CODE_OF_CONDUCT
                | packets::CONFIGURATION_DISCONNECT
        ),
        Phase::Play => matches!(
            id,
            PlayKeepAlive::ID
                | packets::PLAY_PING
                | SynchronizePosition::ID
                | ChunkBatchFinished::ID
                | StartConfiguration::ID
                | PlayPong::ID
                | packets::PLAY_DISCONNECT
        ),
    }
}

fn parse<P: Decode>(body: Option<&[u8]>) -> Result<P> {
    let mut body = body.context("packet body missing")?;
    Ok(P::decode(&mut body)?)
}

/// A cheap, deterministic spread in `[0, 1)` from a bot's index, so bots don't tick in lockstep.
fn spread(index: u32, salt: u64) -> f64 {
    let mut value = u64::from(index) ^ salt.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    #[allow(clippy::cast_precision_loss)]
    let unit = (value >> 11) as f64 / (1_u64 << 53) as f64;
    unit
}

struct Bot {
    index: u32,
    config: Arc<Config>,
    stats: Arc<Stats>,
    stream: TcpStream,
    /// When the bot must be in play by.
    deadline: Instant,
    decoder: wire::Decoder,
    encoder: wire::Encoder,
    phase: Phase,
    /// Held while logging in, for the per-address pending limit.
    permit: Option<OwnedSemaphorePermit>,
    connected: Instant,
    spawned: bool,
    /// Whether the server has placed the bot since it last entered play.
    positioned: bool,
    center: [f64; 3],
    angle: f64,
    pings: Pings,
    next_ping: Instant,
    next_command: Instant,
}

/// Connects to the first of `addresses` that accepts, so an unreachable IPv6 address doesn't hide a working IPv4 one.
async fn connect(addresses: &[SocketAddr]) -> std::io::Result<TcpStream> {
    let mut failure = None;
    for &address in addresses {
        match TcpStream::connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => failure = Some(error),
        }
    }
    Err(failure.unwrap_or_else(|| std::io::ErrorKind::AddrNotAvailable.into()))
}

/// Runs bot `index` until `stop`, recording how it ended.
pub async fn run(
    index: u32,
    config: Arc<Config>,
    addresses: Arc<[SocketAddr]>,
    stats: Arc<Stats>,
    permit: OwnedSemaphorePermit,
    stop: CancellationToken,
) {
    stats.started.fetch_add(1, Relaxed);
    let connected = Instant::now();
    let mut spawned = false;
    let result = async {
        let deadline = connected + Duration::from_secs(config.login_timeout);
        let stream = tokio::select! {
            () = stop.cancelled() => return Ok(()),
            stream = timeout_at(deadline, connect(&addresses)) => {
                stream.context("connect timed out")?.context("connect")?
            }
        };
        stream.set_nodelay(true)?;
        let offset = |salt, period: f64| connected + Duration::from_secs_f64(period * spread(index, salt));
        let mut bot = Bot {
            index,
            stream,
            deadline,
            decoder: wire::Decoder::new(),
            encoder: wire::Encoder::new(),
            phase: Phase::Login,
            permit: Some(permit),
            connected,
            spawned: false,
            positioned: false,
            center: [0.0; 3],
            angle: std::f64::consts::TAU * spread(index, 1),
            pings: Pings::default(),
            next_ping: offset(2, config.ping_interval),
            next_command: offset(3, config.command_interval),
            config: config.clone(),
            stats: stats.clone(),
        };
        let result = tokio::select! {
            () = stop.cancelled() => Ok(()),
            result = bot.drive() => result,
        };
        spawned = bot.spawned;
        bot.leave_play();
        result
    }
    .await;
    stats.ended(spawned, result.err().map(|error| format!("{error:#}")));
}

impl Bot {
    /// Plays until the connection fails, or until the deadline if the bot isn't in play by then.
    async fn drive(&mut self) -> Result<()> {
        let name = self.config.name(self.index);
        self.encoder.packet(&Handshake {
            protocol_version: VarInt(VERSION.protocol),
            server_address: McString::new(self.config.hostname())?,
            server_port: self.stream.peer_addr()?.port(),
            next_state: VarInt(2),
        })?;
        self.encoder.packet(&LoginStart { username: McString::new(&name)?, player_uuid: Uuid([0; 16]) })?;
        self.flush().await?;
        let period = Duration::from_secs(1) / self.config.move_hz.max(1);
        let mut ticks = interval_at(self.connected + period.mul_f64(spread(self.index, 4)), period);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = sleep_until(self.deadline), if !self.spawned => bail!("not in play after {}s", self.config.login_timeout),
                read = self.stream.read_buf(&mut self.decoder.buffer) => {
                    let read = read.context("read")?;
                    if read == 0 {
                        bail!("connection closed");
                    }
                    self.stats.bytes_in.fetch_add(read as u64, Relaxed);
                    loop {
                        let phase = self.phase;
                        let Some(frame) = self.decoder.next(|id| wanted(phase, id))? else { break };
                        self.handle(&frame)?;
                    }
                }
                now = ticks.tick(), if self.positioned => self.tick(now)?,
            }
            self.flush().await?;
        }
    }

    /// Writes what's queued, within the login deadline until the bot has spawned and `WRITE_TIMEOUT` after.
    async fn flush(&mut self) -> Result<()> {
        if !self.encoder.output.is_empty() {
            let (until, failure) = if self.spawned {
                (Instant::now() + WRITE_TIMEOUT, "write timed out".to_string())
            } else {
                (self.deadline, format!("not in play after {}s", self.config.login_timeout))
            };
            timeout_at(until, self.stream.write_all(&self.encoder.output)).await.context(failure)??;
            self.encoder.output.clear();
        }
        Ok(())
    }

    fn handle(&mut self, frame: &wire::Frame) -> Result<()> {
        let body = frame.body.as_deref();
        match (self.phase, frame.id) {
            (Phase::Login, SetCompression::ID) => {
                let threshold = parse::<SetCompression>(body)?.threshold.0;
                self.encoder.enable_compression(usize::try_from(threshold)?);
                self.decoder.enable_compression();
            }
            (Phase::Login, LoginSuccess::ID) => {
                parse::<LoginSuccess>(body)?;
                self.encoder.packet(&LoginAcknowledged)?;
                self.phase = Phase::Configuration;
                self.settings()?;
                self.permit = None;
                self.stats.logged_in.fetch_add(1, Relaxed);
            }
            (Phase::Login, EncryptionRequest::ID) => bail!("server requested encryption; enable offline logins"),
            (Phase::Login, LoginDisconnect::ID) => {
                bail!("kicked in login: {}", parse::<LoginDisconnect>(body)?.reason.as_str())
            }
            (Phase::Configuration, ConfigurationKeepAlive::ID) => {
                let keep_alive_id = parse::<ConfigurationKeepAlive>(body)?.keep_alive_id;
                self.encoder.packet(&ConfigurationKeepAliveResponse { keep_alive_id })?;
            }
            (Phase::Configuration, ConfigurationPing::ID) => {
                self.encoder.packet(&ConfigurationPong { id: parse::<ConfigurationPing>(body)?.id })?;
            }
            (Phase::Configuration, SelectKnownPacks::ID) => {
                self.encoder.packet(&KnownPacks { packs: BoundedArray::new(Vec::new())? })?;
            }
            (Phase::Configuration, FinishConfiguration::ID) => {
                self.encoder.packet(&AcknowledgeConfiguration)?;
                self.phase = Phase::Play;
            }
            (Phase::Configuration, packets::CODE_OF_CONDUCT) => {
                self.encoder.raw(packets::ACCEPT_CODE_OF_CONDUCT, &[])?;
            }
            (Phase::Play, PlayKeepAlive::ID) => {
                let keep_alive_id = parse::<PlayKeepAlive>(body)?.keep_alive_id;
                self.encoder.packet(&PlayKeepAliveResponse { keep_alive_id })?;
            }
            (Phase::Play, packets::PLAY_PING) => {
                self.encoder.raw(packets::PLAY_PONG, body.context("ping body missing")?)?;
            }
            (Phase::Play, SynchronizePosition::ID) => self.teleport(&parse::<SynchronizePosition>(body)?)?,
            (Phase::Play, ChunkBatchFinished::ID) => {
                self.encoder.packet(&ChunkBatchReceived { chunks_per_tick: 20.0 })?;
            }
            (Phase::Play, StartConfiguration::ID) => {
                self.encoder.packet(&ConfigurationAcknowledged)?;
                self.phase = Phase::Configuration;
                self.leave_play();
                self.pings.clear();
                self.settings()?;
                self.stats.reconfigurations.fetch_add(1, Relaxed);
            }
            (Phase::Play, PlayPong::ID) => {
                if let Some(rtt) = self.pings.answered(parse::<PlayPong>(body)?.id, Instant::now()) {
                    self.stats.pong(rtt);
                }
            }
            (Phase::Configuration | Phase::Play, packets::CONFIGURATION_DISCONNECT | packets::PLAY_DISCONNECT)
                if wanted(self.phase, frame.id) =>
            {
                let reason = packets::component_text(body.unwrap_or_default());
                return Err(anyhow!("kicked: {}", reason.chars().take(80).collect::<String>()));
            }
            _ => {}
        }
        Ok(())
    }

    fn leave_play(&mut self) {
        if self.positioned {
            self.positioned = false;
            self.stats.left_play();
        }
    }

    fn settings(&mut self) -> Result<()> {
        self.encoder.packet(&ConfigurationClientInformation {
            locale: McString::new("en_us")?,
            view_distance: self.config.view_distance,
            chat_flags: VarInt(0),
            chat_colors: true,
            skin_parts: 0x7f,
            main_hand: VarInt(1),
            enable_text_filtering: false,
            enable_server_listing: true,
            particle_status: ConfigurationClientInformationParticleStatus::All,
        })?;
        Ok(())
    }

    fn teleport(&mut self, position: &SynchronizePosition) -> Result<()> {
        let target = [position.x, position.y, position.z];
        for (axis, value) in target.into_iter().enumerate() {
            // Bits 0-2 mark coordinates relative to the current position.
            let relative = position.flags & (1 << axis) != 0;
            self.center[axis] = if relative { self.center[axis] + value } else { value };
        }
        self.encoder.packet(&ConfirmTeleport { teleport_id: position.teleport_id })?;
        self.encoder.packet(&PlayerLoaded)?;
        if !self.positioned {
            self.positioned = true;
            self.stats.entered_play();
        }
        if !self.spawned {
            self.spawned = true;
            self.stats.spawned(self.connected.elapsed());
        }
        Ok(())
    }

    /// Walks, pings and runs the command as each falls due.
    fn tick(&mut self, now: Instant) -> Result<()> {
        self.stats.ping_timeouts.fetch_add(self.pings.expire(Instant::now()), Relaxed);
        if self.config.move_hz > 0 {
            self.angle += SPEED / RADIUS / f64::from(self.config.move_hz);
            let [x, y, z] = self.center;
            let (sin, cos) = self.angle.sin_cos();
            self.encoder.packet(&MovePosition { x: x + RADIUS * cos, y, z: z + RADIUS * sin, flags: 1 })?;
        }
        if let Some(period) = self.config.ping_period()
            && now >= self.next_ping
        {
            self.next_ping = now + period;
            // Stamped when sent, not at the scheduled tick, which can run late.
            let (id, pushed_out) = self.pings.sent(Instant::now());
            self.encoder.packet(&PlayPing { id })?;
            self.stats.pings.fetch_add(1, Relaxed);
            self.stats.ping_timeouts.fetch_add(pushed_out, Relaxed);
        }
        if let Some(command) = &self.config.command
            && now >= self.next_command
        {
            self.next_command = now + Duration::from_secs_f64(self.config.command_interval);
            self.encoder.packet(&UnsignedCommand { command: McString::new(command)? })?;
            self.stats.commands.fetch_add(1, Relaxed);
        }
        Ok(())
    }
}

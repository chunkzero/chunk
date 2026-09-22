use super::super::transport::{PreparedPackets, invalid_data};
use super::world::{JoinLimbo, LimboChunk, PreparingTitle, SPAWN};
use chunk_protocol::{
    BoundedArray, McString, VarInt,
    versions::v26_2::{
        ChunkBatchFinished, ChunkBatchStart, FeatureFlags, FinishConfiguration, GameEvent, GameEventReason,
        LIMBO_REGISTRIES, LIMBO_TAGS, PlayerAbilities, SelectKnownPacks, SetChunkCenter, SynchronizePosition,
        TitleTimes,
    },
};
use std::{collections::BTreeMap, io};

/// Each listener owns one compression setting and one packet set per protocol version.
pub(in crate::server) struct Cache(BTreeMap<i32, Packets>);

impl Cache {
    pub(in crate::server) fn new(compression: Option<usize>) -> io::Result<Self> {
        let mut versions = BTreeMap::new();
        for version in chunk_protocol::versions::SUPPORTED {
            versions.insert(version.protocol, Packets::new(version.protocol, compression)?);
        }
        Ok(Self(versions))
    }

    pub(super) fn get(&self, protocol_version: i32) -> io::Result<&Packets> {
        self.0.get(&protocol_version).ok_or_else(|| invalid_data("no limbo packets for negotiated protocol version"))
    }
}

pub(super) struct Packets {
    pub known_packs: PreparedPackets,
    pub configuration: PreparedPackets,
    pub spawn: PreparedPackets,
    pub title: PreparedPackets,
}

impl Packets {
    fn new(protocol_version: i32, compression: Option<usize>) -> io::Result<Self> {
        if protocol_version != chunk_protocol::versions::v26_2::VERSION.protocol {
            return Err(invalid_data("unsupported limbo packet version"));
        }
        let mut known_packs = PreparedPackets::new(compression);
        known_packs.push(&SelectKnownPacks { packs: BoundedArray::new(vec![]).map_err(invalid_data)? })?;
        let mut configuration = PreparedPackets::new(compression);
        configuration.push(&FeatureFlags {
            features: BoundedArray::new(vec![McString::new("minecraft:vanilla").map_err(invalid_data)?])
                .map_err(invalid_data)?,
        })?;
        for frame in LIMBO_REGISTRIES.iter().copied().chain(std::iter::once(LIMBO_TAGS)) {
            configuration.push_frame(frame)?;
        }
        configuration.push(&FinishConfiguration)?;
        let mut spawn = PreparedPackets::new(compression);
        spawn.push(&JoinLimbo)?;
        spawn.push(&PlayerAbilities { flags: 7, flying_speed: 0.0, walking_speed: 0.0 })?;
        spawn.push(&SetChunkCenter { chunk_x: VarInt(0), chunk_z: VarInt(0) })?;
        spawn.push(&GameEvent { reason: GameEventReason::LevelChunksLoadStart, value: 0.0 })?;
        spawn.push(&ChunkBatchStart)?;
        for x in -2..=2 {
            for z in -2..=2 {
                spawn.push(&LimboChunk { x, z })?;
            }
        }
        spawn.push(&ChunkBatchFinished { batch_size: VarInt(25) })?;
        spawn.push(&position())?;
        let mut title = PreparedPackets::new(compression);
        title.push(&TitleTimes { fade_in: 10, stay: 200, fade_out: 20 })?;
        title.push(&PreparingTitle)?;
        Ok(Self { known_packs, configuration, spawn, title })
    }
}

fn position() -> SynchronizePosition {
    SynchronizePosition {
        teleport_id: VarInt(0),
        x: SPAWN[0],
        y: SPAWN[1],
        z: SPAWN[2],
        dx: 0.0,
        dy: 0.0,
        dz: 0.0,
        yaw: 0.0,
        pitch: 0.0,
        flags: 0,
    }
}

#[cfg(test)]
mod tests;

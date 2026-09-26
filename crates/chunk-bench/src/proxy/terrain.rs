//! A synthetic overworld column laid out like vanilla's chunk-with-light packet, so its
//! size and compressibility resemble real chunk traffic. Block IDs are representative only.

const SECTIONS: usize = 24;
const MIN_Y: i32 = -64;
const AIR: u32 = 0;
const STONE: u32 = 1;
const DIRT: u32 = 10;
const GRASS: u32 = 9;
const LAVA: u32 = 102;
const BEDROCK: u32 = 85;
const DEEPSLATE: u32 = 25_965;
const PLANTS: [u32; 3] = [2_050, 2_128, 2_140];
const STONE_BLOBS: [u32; 5] = [2, 4, 6, 10, 124];
const DEEP_BLOBS: [u32; 2] = [22_451, 124];
const STONE_ORES: [u32; 4] = [133, 131, 22_940, 129];
const DEEP_ORES: [u32; 5] = [134, 132, 5_830, 520, 4_720];

/// Packet body: opaque ID byte, chunk coordinates, heightmaps, sections, and light.
pub fn column(seed: u64) -> Vec<u8> {
    let noise = Noise(seed);
    let surface: Vec<i32> = (0..256).map(|i| 64 + noise.smooth([i % 16, 0, i / 16], 24, 1) * 12 / 1024).collect();
    let mut out = vec![0x7f];
    out.extend_from_slice(&[0; 8]);
    varint(&mut out, 2);
    for kind in [1, 4] {
        varint(&mut out, kind);
        let heights: Vec<u64> = surface.iter().map(|&y| u64::try_from(y - MIN_Y + 1).unwrap_or(0)).collect();
        varint(&mut out, heights.len().div_ceil(7));
        longs(&mut out, &heights, 9);
    }
    let mut sections = Vec::new();
    for section in 0..SECTIONS {
        let base = MIN_Y + 16 * i32::try_from(section).unwrap_or(0);
        let blocks: Vec<u32> =
            (0..4096).map(|i| block(&noise, &surface, i % 16, base + i / 256, (i / 16) % 16)).collect();
        let solid = blocks.iter().filter(|&&b| b != AIR).count();
        let fluid = blocks.iter().filter(|&&b| b == LAVA).count();
        sections.extend_from_slice(&u16::try_from(solid).unwrap_or(0).to_be_bytes());
        sections.extend_from_slice(&u16::try_from(fluid).unwrap_or(0).to_be_bytes());
        paletted(&mut sections, &blocks, 4);
        paletted(&mut sections, &[1; 64], 1);
    }
    varint(&mut out, sections.len());
    out.extend_from_slice(&sections);
    varint(&mut out, 0);
    light(&mut out, &surface);
    out
}

fn block(noise: &Noise, surface: &[i32], x: i32, y: i32, z: i32) -> u32 {
    let top = surface[usize::try_from(z * 16 + x).unwrap_or(0)];
    let at = [x, y, z];
    if y > top {
        return if y == top + 1 && noise.hash(at, 7).is_multiple_of(8) { PLANTS[noise.pick(at, 8, 3)] } else { AIR };
    }
    if y < MIN_Y + 5 && noise.hash(at, 1) % 5 >= u64::try_from(y - MIN_Y).unwrap_or(0) {
        return BEDROCK;
    }
    if y < top - 4 && noise.smooth(at, 12, 2) > 696 {
        return if y < -54 { LAVA } else { AIR };
    }
    let deep = y < 0;
    if noise.smooth(at, 6, 3) > 737 {
        return if deep { DEEP_BLOBS[noise.pick(at, 4, 2)] } else { STONE_BLOBS[noise.pick(at, 4, 5)] };
    }
    if noise.smooth(at, 3, 5) > 849 {
        return if deep { DEEP_ORES[noise.pick(at, 6, 5)] } else { STONE_ORES[noise.pick(at, 6, 4)] };
    }
    match top - y {
        0 => GRASS,
        1..=3 => DIRT,
        _ if deep => DEEPSLATE,
        _ => STONE,
    }
}

/// Sky light for lit sections up to one above the highest block, as Minestom sends it; block light is dark.
fn light(out: &mut Vec<u8>, surface: &[i32]) {
    let (lowest, highest) = (surface.iter().min().copied().unwrap_or(0), surface.iter().max().copied().unwrap_or(0));
    // Light sections include one border section below and above the world.
    let base = |section: usize| MIN_Y + 16 * (i32::try_from(section).unwrap_or(0) - 1);
    let lit: Vec<usize> =
        (0..SECTIONS + 2).filter(|&s| base(s) + 15 > lowest - 15 && base(s) <= highest + 16).collect();
    let sky = lit.iter().fold(0_u64, |mask, s| mask | 1 << s);
    let all = (1_u64 << (SECTIONS + 2)) - 1;
    for bits in [sky, 0, all & !sky, all] {
        varint(out, usize::from(bits != 0));
        if bits != 0 {
            out.extend_from_slice(&bits.to_be_bytes());
        }
    }
    varint(out, lit.len());
    for section in lit {
        let mut nibbles = [0_u8; 2048];
        for i in 0..4096 {
            let (x, y, z) = (i % 16, base(section) + i32::try_from(i / 256).unwrap_or(0), (i / 16) % 16);
            let level = (15 - (surface[z * 16 + x] - y).max(0)).max(0);
            nibbles[i / 2] |= u8::try_from(level).unwrap_or(0) << (4 * (i % 2));
        }
        varint(out, nibbles.len());
        out.extend_from_slice(&nibbles);
    }
    varint(out, 0);
}

/// A paletted container: single value, or an indirect palette with at least `min_bits` per entry.
fn paletted(out: &mut Vec<u8>, values: &[u32], min_bits: u32) {
    let mut palette: Vec<u32> = Vec::new();
    let indices: Vec<u64> = values
        .iter()
        .map(|value| {
            let index = palette.iter().position(|p| p == value).unwrap_or_else(|| {
                palette.push(*value);
                palette.len() - 1
            });
            index as u64
        })
        .collect();
    if palette.len() == 1 {
        out.push(0);
        varint(out, palette[0] as usize);
        return;
    }
    let bits = (usize::BITS - (palette.len() - 1).leading_zeros()).max(min_bits);
    out.push(u8::try_from(bits).unwrap_or(0));
    varint(out, palette.len());
    for value in palette {
        varint(out, value as usize);
    }
    longs(out, &indices, bits);
}

/// Packs values into big-endian longs without spanning, as the protocol does; unprefixed.
fn longs(out: &mut Vec<u8>, values: &[u64], bits: u32) {
    for chunk in values.chunks(usize::try_from(64 / bits).unwrap_or(1)) {
        let packed = chunk.iter().zip(0..).fold(0_u64, |long, (&v, i): (&u64, u32)| long | v << (i * bits));
        out.extend_from_slice(&packed.to_be_bytes());
    }
}

fn varint(out: &mut Vec<u8>, value: usize) {
    let mut value = u32::try_from(value).unwrap_or(0);
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Deterministic lattice value noise.
struct Noise(u64);

impl Noise {
    fn hash(&self, [x, y, z]: [i32; 3], salt: u64) -> u64 {
        let mut h = self.0 ^ salt.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        for v in [x, y, z] {
            h = (h ^ u64::from(v.cast_unsigned())).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            h ^= h >> 31;
        }
        h
    }

    fn pick(&self, at: [i32; 3], scale: i32, choices: usize) -> usize {
        let index = self.hash(at.map(|v| v.div_euclid(scale)), 11) % choices as u64;
        usize::try_from(index).unwrap_or(0)
    }

    /// Trilinear value noise in 0..1024 with lattice spacing `scale`.
    fn smooth(&self, at: [i32; 3], scale: i32, salt: u64) -> i32 {
        let cell = at.map(|v| v.div_euclid(scale));
        let frac = at.map(|v| i64::from(v.rem_euclid(scale)));
        let mut total = 0;
        for corner in 0..8 {
            let offset = [corner & 1, (corner >> 1) & 1, (corner >> 2) & 1];
            let point = [cell[0] + offset[0], cell[1] + offset[1], cell[2] + offset[2]];
            let weight: i64 =
                (0..3).map(|axis| if offset[axis] == 1 { frac[axis] } else { i64::from(scale) - frac[axis] }).product();
            total += weight * i64::try_from(self.hash(point, salt) % 1024).unwrap_or(0);
        }
        i32::try_from(total / i64::from(scale).pow(3)).unwrap_or(0)
    }
}

use lansec_protocol::Channel;

/// Conservative LAN UDP payload. Ethernet MTU 1500 minus IP/UDP/BUD overhead.
pub const MTU: usize = 1400;
pub const MAX_PAYLOAD: usize = 1200;
pub const MAGIC: [u8; 4] = *b"BUD1";

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    Hello = 1,
    HelloAck = 2,
    Data = 10,
    Ack = 11,
    Nack = 12,
}

impl PacketType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Hello),
            2 => Some(Self::HelloAck),
            10 => Some(Self::Data),
            11 => Some(Self::Ack),
            12 => Some(Self::Nack),
            _ => None,
        }
    }
}

/// Cleartext BUD header. Encrypted payload follows for Data/Ack/Nack after handshake.
#[derive(Debug, Clone, Copy)]
pub struct PacketHeader {
    pub ty: PacketType,
    pub channel: Channel,
    pub flags: u8,
    pub seq: u32,
    pub frame_id: u32,
    pub frag_idx: u16,
    pub frag_count: u16,
}

pub const FLAG_KEYFRAME: u8 = 1 << 0;
pub const FLAG_RELIABLE: u8 = 1 << 1;
pub const FLAG_FIN: u8 = 1 << 2;

pub const HEADER_LEN: usize = 4 + 1 + 1 + 1 + 1 + 4 + 4 + 2 + 2;

impl PacketHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4] = self.ty as u8;
        buf[5] = self.channel as u8;
        buf[6] = self.flags;
        buf[7] = 0;
        buf[8..12].copy_from_slice(&self.seq.to_le_bytes());
        buf[12..16].copy_from_slice(&self.frame_id.to_le_bytes());
        buf[16..18].copy_from_slice(&self.frag_idx.to_le_bytes());
        buf[18..20].copy_from_slice(&self.frag_count.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        if buf[0..4] != MAGIC {
            return None;
        }
        Some(Self {
            ty: PacketType::from_u8(buf[4])?,
            channel: Channel::from_u8(buf[5])?,
            flags: buf[6],
            seq: u32::from_le_bytes(buf[8..12].try_into().ok()?),
            frame_id: u32::from_le_bytes(buf[12..16].try_into().ok()?),
            frag_idx: u16::from_le_bytes(buf[16..18].try_into().ok()?),
            frag_count: u16::from_le_bytes(buf[18..20].try_into().ok()?),
        })
    }
}

pub fn fragment(payload: &[u8]) -> Vec<(u16, u16, &[u8])> {
    if payload.is_empty() {
        return vec![(0, 1, payload)];
    }
    let count = payload.len().div_ceil(MAX_PAYLOAD) as u16;
    payload
        .chunks(MAX_PAYLOAD)
        .enumerate()
        .map(|(i, chunk)| (i as u16, count, chunk))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = PacketHeader {
            ty: PacketType::Data,
            channel: Channel::Video,
            flags: FLAG_KEYFRAME,
            seq: 42,
            frame_id: 7,
            frag_idx: 1,
            frag_count: 3,
        };
        let bytes = h.encode();
        let back = PacketHeader::decode(&bytes).unwrap();
        assert_eq!(back.seq, 42);
        assert_eq!(back.frag_count, 3);
        assert_eq!(back.flags, FLAG_KEYFRAME);
    }
}

use std::collections::{BTreeMap, HashMap};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use lansec_protocol::Channel;
use parking_lot::Mutex;
use tracing::{debug, warn};

use crate::congestion::CongestionController;
use crate::crypto::{Handshake, SessionKeys};
use crate::packet::{
    fragment, PacketHeader, PacketType, FLAG_FIN, FLAG_KEYFRAME, FLAG_RELIABLE, HEADER_LEN, MAX_PAYLOAD,
};

const RELIABLE_WINDOW: u32 = 1024;
const RETRANSMIT: Duration = Duration::from_millis(8);
const VIDEO_ASSEMBLE_TTL: Duration = Duration::from_millis(80);

#[derive(Debug, Clone)]
pub struct BudConfig {
    pub bind: SocketAddr,
    pub pin: String,
    pub is_host: bool,
}

#[derive(Debug)]
pub enum Incoming {
    Established { peer: SocketAddr },
    Datagram { channel: Channel, payload: Vec<u8>, frame_id: u32, keyframe: bool },
    NeedIdr,
}

struct Outgoing {
    #[allow(dead_code)]
    header: PacketHeader,
    payload: Vec<u8>,
    last_send: Instant,
    first_send: Instant,
    #[allow(dead_code)]
    reliable: bool,
}

struct VideoFrameBuf {
    parts: HashMap<u16, Vec<u8>>,
    count: u16,
    keyframe: bool,
    first_seen: Instant,
}

struct State {
    keys: Option<SessionKeys>,
    handshake: Handshake,
    peer: Option<SocketAddr>,
    established: bool,
    send_seq: u32,
    crypto_nonce: u64,
    reliable_next: u32,
    reliable_out: BTreeMap<u32, Outgoing>,
    reliable_in_next: u32,
    reliable_hold: BTreeMap<u32, (Channel, Vec<u8>)>,
    acked: u32,
    video: HashMap<u32, VideoFrameBuf>,
    last_video_frame: u32,
    sent_packets: u32,
    lost_packets: u32,
}

pub struct BudEndpoint {
    sock: UdpSocket,
    cfg: BudConfig,
    inner: Mutex<State>,
    pub congestion: Mutex<CongestionController>,
}

impl BudEndpoint {
    pub fn bind(cfg: BudConfig) -> std::io::Result<Self> {
        let sock = UdpSocket::bind(cfg.bind)?;
        sock.set_nonblocking(true)?;
        let is_host = cfg.is_host;
        Ok(Self {
            sock,
            cfg,
            inner: Mutex::new(State {
                keys: None,
                handshake: Handshake::new(),
                peer: None,
                established: false,
                send_seq: 1,
                crypto_nonce: if is_host { 1 } else { 1u64 << 63 },
                reliable_next: 1,
                reliable_out: BTreeMap::new(),
                reliable_in_next: 1,
                reliable_hold: BTreeMap::new(),
                acked: 0,
                video: HashMap::new(),
                last_video_frame: 0,
                sent_packets: 0,
                lost_packets: 0,
            }),
            congestion: Mutex::new(CongestionController::lan_default()),
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    pub fn connect(&self, peer: SocketAddr) -> std::io::Result<()> {
        {
            let mut st = self.inner.lock();
            st.peer = Some(peer);
        }
        self.send_hello(peer)
    }

    fn send_hello(&self, peer: SocketAddr) -> std::io::Result<()> {
        let st = self.inner.lock();
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&PacketHeader {
            ty: if self.cfg.is_host {
                PacketType::HelloAck
            } else {
                PacketType::Hello
            },
            channel: Channel::Control,
            flags: 0,
            seq: 0,
            frame_id: 0,
            frag_idx: 0,
            frag_count: 1,
        }
        .encode());
        buf.extend_from_slice(st.handshake.public.as_bytes());
        buf.extend_from_slice(&st.handshake.nonce);
        drop(st);
        self.sock.send_to(&buf, peer)?;
        Ok(())
    }

    pub fn send(&self, channel: Channel, payload: &[u8], frame_id: u32, keyframe: bool) -> std::io::Result<()> {
        let reliable = channel.is_reliable() || (channel == Channel::Video && keyframe);
        let frags = fragment(payload);
        let count = frags.len() as u16;
        for (idx, _, chunk) in frags {
            self.send_one(channel, chunk, frame_id, idx, count, reliable, keyframe)?;
        }
        Ok(())
    }

    fn send_one(
        self: &Self,
        channel: Channel,
        chunk: &[u8],
        frame_id: u32,
        frag_idx: u16,
        frag_count: u16,
        reliable: bool,
        keyframe: bool,
    ) -> std::io::Result<()> {
        let peer = {
            let st = self.inner.lock();
            st.peer.ok_or_else(|| std::io::Error::other("no peer"))?
        };
        let mut flags = 0;
        if reliable {
            flags |= FLAG_RELIABLE;
        }
        if keyframe {
            flags |= FLAG_KEYFRAME;
        }
        if frag_idx + 1 == frag_count {
            flags |= FLAG_FIN;
        }
        let (header, packet) = {
            let mut st = self.inner.lock();
            let keys = st
                .keys
                .clone()
                .ok_or_else(|| std::io::Error::other("not established"))?;
            let seq = if reliable {
                let s = st.reliable_next;
                st.reliable_next = st.reliable_next.wrapping_add(1);
                s
            } else {
                let s = st.send_seq;
                st.send_seq = st.send_seq.wrapping_add(1);
                s
            };
            let header = PacketHeader {
                ty: PacketType::Data,
                channel,
                flags,
                seq,
                frame_id,
                frag_idx,
                frag_count,
            };
            let nonce = st.crypto_nonce;
            st.crypto_nonce += 1;
            let hdr_bytes = header.encode();
            let ct = keys
                .seal(nonce, &hdr_bytes, chunk)
                .map_err(|e| std::io::Error::other(e))?;
            let mut packet = Vec::with_capacity(HEADER_LEN + 8 + ct.len());
            packet.extend_from_slice(&hdr_bytes);
            packet.extend_from_slice(&nonce.to_le_bytes());
            packet.extend_from_slice(&ct);
            if reliable {
                let now = Instant::now();
                st.reliable_out.insert(
                    seq,
                    Outgoing {
                        header,
                        payload: packet.clone(),
                        last_send: now,
                        first_send: now,
                        reliable: true,
                    },
                );
            }
            st.sent_packets += 1;
            (header, packet)
        };
        let _ = header;
        self.sock.send_to(&packet, peer)?;
        Ok(())
    }

    pub fn poll(&self, out: &mut Vec<Incoming>) -> std::io::Result<()> {
        let mut buf = [0u8; 2048];
        loop {
            match self.sock.recv_from(&mut buf) {
                Ok((n, from)) => self.handle_datagram(&buf[..n], from, out)?,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        self.retransmit()?;
        self.expire_video(out);
        Ok(())
    }

    fn handle_datagram(&self, buf: &[u8], from: SocketAddr, out: &mut Vec<Incoming>) -> std::io::Result<()> {
        let Some(hdr) = PacketHeader::decode(buf) else {
            return Ok(());
        };
        match hdr.ty {
            PacketType::Hello | PacketType::HelloAck => self.on_hello(buf, from, hdr.ty, out),
            PacketType::Ack => {
                self.on_ack(buf);
                Ok(())
            }
            PacketType::Nack => {
                self.on_nack(buf, out);
                Ok(())
            }
            PacketType::Data => self.on_data(buf, hdr, from, out),
        }
    }

    fn on_hello(
        &self,
        buf: &[u8],
        from: SocketAddr,
        ty: PacketType,
        out: &mut Vec<Incoming>,
    ) -> std::io::Result<()> {
        if buf.len() < HEADER_LEN + 32 + 16 {
            return Ok(());
        }
        let mut pk = [0u8; 32];
        pk.copy_from_slice(&buf[HEADER_LEN..HEADER_LEN + 32]);
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(&buf[HEADER_LEN + 32..HEADER_LEN + 48]);
        let peer_public = x25519_dalek::PublicKey::from(pk);
        let just_established = {
            let mut st = self.inner.lock();
            st.peer = Some(from);
            let keys = st.handshake.derive(&peer_public, &nonce, &self.cfg.pin);
            st.keys = Some(keys);
            let was = st.established;
            st.established = true;
            !was
        };
        if matches!(ty, PacketType::Hello) && self.cfg.is_host {
            self.send_hello(from)?;
        }
        if just_established {
            out.push(Incoming::Established { peer: from });
        }
        Ok(())
    }

    fn on_data(
        &self,
        buf: &[u8],
        hdr: PacketHeader,
        from: SocketAddr,
        out: &mut Vec<Incoming>,
    ) -> std::io::Result<()> {
        if buf.len() < HEADER_LEN + 8 {
            return Ok(());
        }
        let nonce = u64::from_le_bytes(buf[HEADER_LEN..HEADER_LEN + 8].try_into().unwrap());
        let ct = &buf[HEADER_LEN + 8..];
        let plain = {
            let st = self.inner.lock();
            let Some(keys) = st.keys.as_ref() else {
                return Ok(());
            };
            match keys.open(nonce, &buf[..HEADER_LEN], ct) {
                Ok(p) => p,
                Err(_) => {
                    warn!("decrypt failed seq={}", hdr.seq);
                    return Ok(());
                }
            }
        };
        if hdr.channel == Channel::Video {
            if hdr.flags & FLAG_RELIABLE != 0 {
                self.send_ack(from, hdr.seq)?;
            }
            let assembled = {
                let mut st = self.inner.lock();
                if hdr.frame_id + 8 < st.last_video_frame {
                    None
                } else {
                    let entry = st.video.entry(hdr.frame_id).or_insert(VideoFrameBuf {
                        parts: HashMap::new(),
                        count: hdr.frag_count,
                        keyframe: hdr.flags & FLAG_KEYFRAME != 0,
                        first_seen: Instant::now(),
                    });
                    entry.parts.insert(hdr.frag_idx, plain);
                    entry.count = hdr.frag_count;
                    if entry.parts.len() as u16 == entry.count {
                        let mut full = Vec::new();
                        for i in 0..entry.count {
                            if let Some(p) = entry.parts.get(&i) {
                                full.extend_from_slice(p);
                            }
                        }
                        let kf = entry.keyframe;
                        st.video.remove(&hdr.frame_id);
                        st.last_video_frame = st.last_video_frame.max(hdr.frame_id);
                        Some((full, kf))
                    } else {
                        None
                    }
                }
            };
            if let Some((payload, kf)) = assembled {
                out.push(Incoming::Datagram {
                    channel: Channel::Video,
                    payload,
                    frame_id: hdr.frame_id,
                    keyframe: kf,
                });
            }
            return Ok(());
        }

        if hdr.flags & FLAG_RELIABLE != 0 {
            self.send_ack(from, hdr.seq)?;
            let ready = {
                let mut st = self.inner.lock();
                if hdr.seq < st.reliable_in_next {
                    Vec::new()
                } else {
                    st.reliable_hold.insert(hdr.seq, (hdr.channel, plain));
                    let mut ready = Vec::new();
                    loop {
                        let next = st.reliable_in_next;
                        let Some(item) = st.reliable_hold.remove(&next) else {
                            break;
                        };
                        ready.push(item);
                        st.reliable_in_next = st.reliable_in_next.wrapping_add(1);
                    }
                    ready
                }
            };
            for (ch, payload) in ready {
                out.push(Incoming::Datagram {
                    channel: ch,
                    payload,
                    frame_id: hdr.frame_id,
                    keyframe: hdr.flags & FLAG_KEYFRAME != 0,
                });
            }
            return Ok(());
        }

        out.push(Incoming::Datagram {
            channel: hdr.channel,
            payload: plain,
            frame_id: hdr.frame_id,
            keyframe: false,
        });
        Ok(())
    }

    fn send_ack(&self, peer: SocketAddr, seq: u32) -> std::io::Result<()> {
        let hdr = PacketHeader {
            ty: PacketType::Ack,
            channel: Channel::Control,
            flags: 0,
            seq,
            frame_id: 0,
            frag_idx: 0,
            frag_count: 1,
        };
        let mut buf = hdr.encode().to_vec();
        {
            let mut st = self.inner.lock();
            if let Some(keys) = st.keys.clone() {
                let nonce = st.crypto_nonce;
                st.crypto_nonce += 1;
                if let Ok(ct) = keys.seal(nonce, &hdr.encode(), &[]) {
                    buf.extend_from_slice(&nonce.to_le_bytes());
                    buf.extend_from_slice(&ct);
                }
            }
        }
        self.sock.send_to(&buf, peer)?;
        Ok(())
    }

    fn on_ack(&self, buf: &[u8]) {
        let Some(hdr) = PacketHeader::decode(buf) else {
            return;
        };
        let rtt = {
            let mut st = self.inner.lock();
            st.acked = st.acked.max(hdr.seq);
            st.reliable_out.remove(&hdr.seq).map(|pkt| pkt.first_send.elapsed().as_micros() as u32)
        };
        if let Some(rtt) = rtt {
            self.congestion.lock().on_rtt_sample(rtt);
        }
    }

    fn on_nack(&self, buf: &[u8], out: &mut Vec<Incoming>) {
        let Some(hdr) = PacketHeader::decode(buf) else {
            return;
        };
        let mut st = self.inner.lock();
        st.lost_packets += 1;
        if hdr.flags & FLAG_KEYFRAME != 0 {
            if let Some(pkt) = st.reliable_out.get(&hdr.seq) {
                let _ = pkt;
            }
        } else {
            out.push(Incoming::NeedIdr);
        }
        let sent = st.sent_packets;
        let lost = st.lost_packets;
        drop(st);
        self.congestion.lock().on_loss(lost, sent.max(1));
    }

    fn retransmit(&self) -> std::io::Result<()> {
        let now = Instant::now();
        let (peer, packets) = {
            let mut st = self.inner.lock();
            let Some(peer) = st.peer else {
                return Ok(());
            };
            if st.reliable_out.len() > RELIABLE_WINDOW as usize {
                debug!("reliable window full");
            }
            let mut packets = Vec::new();
            for pkt in st.reliable_out.values_mut() {
                if now.duration_since(pkt.last_send) >= RETRANSMIT {
                    pkt.last_send = now;
                    packets.push(pkt.payload.clone());
                }
            }
            (peer, packets)
        };
        for p in packets {
            self.sock.send_to(&p, peer)?;
        }
        Ok(())
    }

    fn expire_video(&self, out: &mut Vec<Incoming>) {
        let now = Instant::now();
        let mut st = self.inner.lock();
        let expired: Vec<u32> = st
            .video
            .iter()
            .filter(|(_, b)| now.duration_since(b.first_seen) > VIDEO_ASSEMBLE_TTL)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(buf) = st.video.remove(&id) {
                st.lost_packets += 1;
                if buf.keyframe {
                    out.push(Incoming::NeedIdr);
                } else {
                    out.push(Incoming::NeedIdr);
                }
            }
        }
    }

    pub fn is_established(&self) -> bool {
        self.inner.lock().established
    }
}

#[allow(dead_code)]
fn _max_payload() -> usize {
    MAX_PAYLOAD
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn loopback_pair() -> (BudEndpoint, BudEndpoint) {
        let a = BudEndpoint::bind(BudConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            pin: "4242".into(),
            is_host: true,
        })
        .unwrap();
        let b = BudEndpoint::bind(BudConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            pin: "4242".into(),
            is_host: false,
        })
        .unwrap();
        (a, b)
    }

    #[test]
    fn handshake_and_unreliable_video() {
        let (host, client) = loopback_pair();
        let host_addr: SocketAddr = host.local_addr().unwrap();
        client.connect(host_addr).unwrap();
        let mut hin = Vec::new();
        let mut cin = Vec::new();
        for _ in 0..20 {
            host.poll(&mut hin).unwrap();
            client.poll(&mut cin).unwrap();
            if host.is_established() && client.is_established() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(host.is_established() && client.is_established());

        let payload = vec![7u8; 3000];
        host.send(Channel::Video, &payload, 3, true).unwrap();
        let mut got = None;
        for _ in 0..40 {
            client.poll(&mut cin).unwrap();
            host.poll(&mut hin).unwrap();
            if let Some(Incoming::Datagram { payload: p, .. }) =
                cin.iter().find(|m| matches!(m, Incoming::Datagram { channel: Channel::Video, .. }))
            {
                got = Some(p.clone());
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got.as_deref(), Some(payload.as_slice()));
    }

    #[test]
    fn reliable_input_in_order() {
        let (host, client) = loopback_pair();
        client.connect(host.local_addr().unwrap()).unwrap();
        let mut hin = Vec::new();
        let mut cin = Vec::new();
        for _ in 0..20 {
            host.poll(&mut hin).unwrap();
            client.poll(&mut cin).unwrap();
            if host.is_established() && client.is_established() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        client.send(Channel::Input, b"click", 0, false).unwrap();
        let mut got = false;
        for _ in 0..40 {
            host.poll(&mut hin).unwrap();
            client.poll(&mut cin).unwrap();
            if hin.iter().any(|m| matches!(m, Incoming::Datagram { channel: Channel::Input, payload, .. } if payload == b"click")) {
                got = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(got);
    }
}

use std::collections::{BTreeMap, HashMap};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
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
const IDR_DEBOUNCE: Duration = Duration::from_millis(250);
const CHANNELS: usize = 4;

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
    reliable_next: [u32; CHANNELS],
    reliable_out: BTreeMap<(u8, u32), Outgoing>,
    reliable_in_next: [u32; CHANNELS],
    reliable_hold: BTreeMap<(u8, u32), Vec<u8>>,
    acked: u32,
    video: HashMap<u32, VideoFrameBuf>,
    last_video_frame: u32,
    sent_packets: u32,
    lost_packets: u32,
    epoch: u8,
    decrypt_fails: u32,
    last_idr_req: Option<Instant>,
}

/// UDP + queue snapshot since the previous call. Both sides print this ~2 Hz.
#[derive(Debug, Clone, Copy)]
pub struct LinkSnapshot {
    pub dt_s: f32,
    pub send_mbps: f32,
    pub recv_mbps: f32,
    pub send_pps: f32,
    pub recv_pps: f32,
    pub would_block: u64,
    pub reliable_out: usize,
    pub reliable_hold: usize,
    pub video_partial: usize,
    pub rtt_us: u32,
    pub loss_ppm: u32,
    pub target_mbps: f32,
}

struct LinkMeter {
    t0: Instant,
    sent: u64,
    recv: u64,
    pkts_sent: u64,
    pkts_recv: u64,
    would_block: u64,
}

pub struct BudEndpoint {
    sock: UdpSocket,
    cfg: BudConfig,
    inner: Mutex<State>,
    pub congestion: Mutex<CongestionController>,
    bytes_sent: AtomicU64,
    bytes_recv: AtomicU64,
    pkts_sent: AtomicU64,
    pkts_recv: AtomicU64,
    would_block: AtomicU64,
    meter: Mutex<LinkMeter>,
}

impl BudEndpoint {
    pub fn bind(cfg: BudConfig) -> std::io::Result<Self> {
        let sock = UdpSocket::bind(cfg.bind)?;
        sock.set_nonblocking(true)?;
        bump_udp_buffers(&sock);
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
                reliable_next: [1; CHANNELS],
                reliable_out: BTreeMap::new(),
                reliable_in_next: [1; CHANNELS],
                reliable_hold: BTreeMap::new(),
                acked: 0,
                video: HashMap::new(),
                last_video_frame: 0,
                sent_packets: 0,
                lost_packets: 0,
                epoch: 0,
                decrypt_fails: 0,
                last_idr_req: None,
            }),
            congestion: Mutex::new(CongestionController::lan_default()),
            bytes_sent: AtomicU64::new(0),
            bytes_recv: AtomicU64::new(0),
            pkts_sent: AtomicU64::new(0),
            pkts_recv: AtomicU64::new(0),
            would_block: AtomicU64::new(0),
            meter: Mutex::new(LinkMeter {
                t0: Instant::now(),
                sent: 0,
                recv: 0,
                pkts_sent: 0,
                pkts_recv: 0,
                would_block: 0,
            }),
        })
    }

    /// Bytes/packets since the last snapshot. Safe to call from one thread per endpoint.
    pub fn snapshot_link(&self) -> LinkSnapshot {
        fn mbps(bytes: u64, dt: f32) -> f32 {
            bytes as f32 * 8.0 / dt / 1_000_000.0
        }
        let sent = self.bytes_sent.load(Ordering::Relaxed);
        let recv = self.bytes_recv.load(Ordering::Relaxed);
        let ps = self.pkts_sent.load(Ordering::Relaxed);
        let pr = self.pkts_recv.load(Ordering::Relaxed);
        let wb = self.would_block.load(Ordering::Relaxed);
        let cong = self.congestion.lock().stats();
        let (reliable_out, reliable_hold, video_partial) = {
            let st = self.inner.lock();
            (st.reliable_out.len(), st.reliable_hold.len(), st.video.len())
        };
        let mut meter = self.meter.lock();
        let dt = meter.t0.elapsed().as_secs_f32().max(0.001);
        let snap = LinkSnapshot {
            dt_s: dt,
            send_mbps: mbps(sent.saturating_sub(meter.sent), dt),
            recv_mbps: mbps(recv.saturating_sub(meter.recv), dt),
            send_pps: ps.saturating_sub(meter.pkts_sent) as f32 / dt,
            recv_pps: pr.saturating_sub(meter.pkts_recv) as f32 / dt,
            would_block: wb.saturating_sub(meter.would_block),
            reliable_out,
            reliable_hold,
            video_partial,
            rtt_us: cong.rtt_us,
            loss_ppm: cong.loss_ppm,
            target_mbps: cong.target_bps as f32 / 1_000_000.0,
        };
        meter.t0 = Instant::now();
        meter.sent = sent;
        meter.recv = recv;
        meter.pkts_sent = ps;
        meter.pkts_recv = pr;
        meter.would_block = wb;
        snap
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
            epoch: st.epoch,
            seq: 0,
            frame_id: 0,
            frag_idx: 0,
            frag_count: 1,
        }
        .encode());
        buf.extend_from_slice(st.handshake.public.as_bytes());
        buf.extend_from_slice(&st.handshake.nonce);
        drop(st);
        self.send_udp(&buf, peer)?;
        Ok(())
    }

    pub fn send(&self, channel: Channel, payload: &[u8], frame_id: u32, keyframe: bool) -> std::io::Result<()> {
        self.send_with(channel, payload, frame_id, keyframe, channel.is_reliable())
    }

    /// Same as [`send`], but reliability is explicit. Mouse moves must be
    /// unreliable: a lost packet must not stall later clicks behind in-order seq.
    pub fn send_with(
        self: &Self,
        channel: Channel,
        payload: &[u8],
        frame_id: u32,
        keyframe: bool,
        reliable: bool,
    ) -> std::io::Result<()> {
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
                let i = channel.idx();
                let s = st.reliable_next[i];
                st.reliable_next[i] = st.reliable_next[i].wrapping_add(1);
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
                epoch: st.epoch,
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
                    (channel as u8, seq),
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
        self.send_udp(&packet, peer)?;
        Ok(())
    }

    fn send_udp(&self, buf: &[u8], peer: SocketAddr) -> std::io::Result<()> {
        match self.sock.send_to(buf, peer) {
            Ok(n) => {
                self.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
                self.pkts_sent.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                self.would_block.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub fn poll(&self, out: &mut Vec<Incoming>) -> std::io::Result<()> {
        let mut buf = [0u8; 2048];
        loop {
            match self.sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    self.bytes_recv.fetch_add(n as u64, Ordering::Relaxed);
                    self.pkts_recv.fetch_add(1, Ordering::Relaxed);
                    self.handle_datagram(&buf[..n], from, out)?;
                }
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
            PacketType::Hello | PacketType::HelloAck => self.on_hello(buf, from, hdr, out),
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

    fn reset_transport(st: &mut State, is_host: bool) {
        st.reliable_out.clear();
        st.reliable_hold.clear();
        st.video.clear();
        st.send_seq = 1;
        st.reliable_next = [1; CHANNELS];
        st.reliable_in_next = [1; CHANNELS];
        st.acked = 0;
        st.last_video_frame = 0;
        st.sent_packets = 0;
        st.lost_packets = 0;
        st.crypto_nonce = if is_host { 1 } else { 1u64 << 63 };
        st.decrypt_fails = 0;
        st.last_idr_req = None;
        st.keys = None;
    }

    fn on_hello(
        &self,
        buf: &[u8],
        from: SocketAddr,
        hdr: PacketHeader,
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
        let fire_established = {
            let mut st = self.inner.lock();
            let was = st.established;
            if matches!(hdr.ty, PacketType::Hello) && self.cfg.is_host {
                // New client (or reconnect): drop in-flight packets encrypted under the old key.
                st.handshake = Handshake::new();
                st.epoch = st.epoch.wrapping_add(1);
                if st.epoch == 0 {
                    st.epoch = 1;
                }
                Self::reset_transport(&mut st, true);
            } else if matches!(hdr.ty, PacketType::HelloAck) {
                st.epoch = if hdr.epoch == 0 { 1 } else { hdr.epoch };
                Self::reset_transport(&mut st, false);
            }
            st.peer = Some(from);
            st.keys = Some(st.handshake.derive(&peer_public, &nonce, &self.cfg.pin));
            st.established = true;
            matches!(hdr.ty, PacketType::Hello) || !was
        };
        if matches!(hdr.ty, PacketType::Hello) && self.cfg.is_host {
            *self.congestion.lock() = CongestionController::lan_default();
            self.send_hello(from)?;
        }
        if fire_established {
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
            let mut st = self.inner.lock();
            if st.epoch != 0 && hdr.epoch != 0 && hdr.epoch != st.epoch {
                // Previous session still in the network / retransmit queue.
                return Ok(());
            }
            let Some(keys) = st.keys.clone() else {
                return Ok(());
            };
            match keys.open(nonce, &buf[..HEADER_LEN], ct) {
                Ok(p) => p,
                Err(_) => {
                    st.decrypt_fails = st.decrypt_fails.saturating_add(1);
                    if st.decrypt_fails == 1 || st.decrypt_fails % 64 == 0 {
                        warn!(fails = st.decrypt_fails, seq = hdr.seq, epoch = hdr.epoch, "decrypt failed");
                    }
                    return Ok(());
                }
            }
        };
        if hdr.channel == Channel::Video {
            if hdr.flags & FLAG_RELIABLE != 0 {
                self.send_ack(from, hdr.channel, hdr.seq)?;
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
            self.send_ack(from, hdr.channel, hdr.seq)?;
            let ch = hdr.channel as u8;
            let i = hdr.channel.idx();
            let ready = {
                let mut st = self.inner.lock();
                if hdr.seq < st.reliable_in_next[i] {
                    Vec::new()
                } else {
                    st.reliable_hold.insert((ch, hdr.seq), plain);
                    let mut ready = Vec::new();
                    loop {
                        let next = st.reliable_in_next[i];
                        let Some(payload) = st.reliable_hold.remove(&(ch, next)) else {
                            break;
                        };
                        ready.push(payload);
                        st.reliable_in_next[i] = st.reliable_in_next[i].wrapping_add(1);
                    }
                    ready
                }
            };
            for payload in ready {
                out.push(Incoming::Datagram {
                    channel: hdr.channel,
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

    fn send_ack(&self, peer: SocketAddr, channel: Channel, seq: u32) -> std::io::Result<()> {
        let hdr = PacketHeader {
            ty: PacketType::Ack,
            channel,
            flags: 0,
            epoch: 0,
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
        self.send_udp(&buf, peer)?;
        Ok(())
    }

    fn on_ack(&self, buf: &[u8]) {
        let Some(hdr) = PacketHeader::decode(buf) else {
            return;
        };
        let rtt = {
            let mut st = self.inner.lock();
            st.acked = st.acked.max(hdr.seq);
            st.reliable_out
                .remove(&(hdr.channel as u8, hdr.seq))
                .map(|pkt| pkt.first_send.elapsed().as_micros() as u32)
        };
        if let Some(rtt) = rtt {
            self.congestion.lock().on_rtt_sample(rtt);
        }
    }

    fn on_nack(&self, buf: &[u8], out: &mut Vec<Incoming>) {
        if PacketHeader::decode(buf).is_none() {
            return;
        }
        let (sent, lost) = {
            let mut st = self.inner.lock();
            st.lost_packets += 1;
            (st.sent_packets, st.lost_packets)
        };
        self.congestion.lock().on_loss(lost, sent.max(1));
        self.push_idr(out);
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
            self.send_udp(&p, peer)?;
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
        let mut need_idr = false;
        for id in expired {
            if st.video.remove(&id).is_some() {
                st.lost_packets += 1;
                need_idr = true;
            }
        }
        drop(st);
        if need_idr {
            let (lost, sent) = {
                let st = self.inner.lock();
                (st.lost_packets, st.sent_packets)
            };
            self.congestion.lock().on_loss(lost, sent.max(1));
            self.push_idr(out);
        }
    }

    fn push_idr(&self, out: &mut Vec<Incoming>) {
        let mut st = self.inner.lock();
        if let Some(prev) = st.last_idr_req {
            if prev.elapsed() < IDR_DEBOUNCE {
                return;
            }
        }
        st.last_idr_req = Some(Instant::now());
        drop(st);
        out.push(Incoming::NeedIdr);
    }

    pub fn is_established(&self) -> bool {
        self.inner.lock().established
    }
}

fn bump_udp_buffers(sock: &UdpSocket) {
    const BYTES: i32 = 8 * 1024 * 1024;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        #[link(name = "ws2_32")]
        extern "system" {
            fn setsockopt(s: usize, level: i32, name: i32, val: *const i8, len: i32) -> i32;
        }
        let s = sock.as_raw_socket() as usize;
        let v = BYTES;
        unsafe {
            let _ = setsockopt(s, 0xffff, 0x1001, &v as *const i32 as *const i8, 4);
            let _ = setsockopt(s, 0xffff, 0x1002, &v as *const i32 as *const i8, 4);
        }
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = sock.as_raw_fd();
        let v = BYTES;
        unsafe {
            let _ = libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &v as *const i32 as *const libc::c_void,
                std::mem::size_of_val(&v) as libc::socklen_t,
            );
            let _ = libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &v as *const i32 as *const libc::c_void,
                std::mem::size_of_val(&v) as libc::socklen_t,
            );
        }
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

    #[test]
    fn unreliable_mouse_delivers_without_acks() {
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
        for i in 0..32u8 {
            client
                .send_with(Channel::Input, &[b'm', i], 0, false, false)
                .unwrap();
        }
        let mut got = 0;
        for _ in 0..40 {
            host.poll(&mut hin).unwrap();
            got = hin
                .iter()
                .filter(|m| matches!(m, Incoming::Datagram { channel: Channel::Input, .. }))
                .count();
            if got >= 32 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(got >= 32, "unreliable mouse must not wait on acks, got {got}");
    }

    #[test]
    fn reconnect_replaces_session_keys() {
        let (host, client1) = loopback_pair();
        let host_addr: SocketAddr = host.local_addr().unwrap();
        client1.connect(host_addr).unwrap();
        let mut hin = Vec::new();
        let mut cin = Vec::new();
        for _ in 0..20 {
            host.poll(&mut hin).unwrap();
            client1.poll(&mut cin).unwrap();
            if host.is_established() && client1.is_established() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(host.is_established() && client1.is_established());
        host.send(Channel::Control, b"stale-reliable", 0, true).unwrap();
        host.poll(&mut hin).unwrap();

        let client2 = BudEndpoint::bind(BudConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            pin: "4242".into(),
            is_host: false,
        })
        .unwrap();
        client2.connect(host_addr).unwrap();
        let mut c2 = Vec::new();
        hin.clear();
        for _ in 0..30 {
            host.poll(&mut hin).unwrap();
            client2.poll(&mut c2).unwrap();
            if client2.is_established()
                && hin
                    .iter()
                    .any(|m| matches!(m, Incoming::Established { .. }))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(client2.is_established());
        assert!(hin.iter().any(|m| matches!(m, Incoming::Established { .. })));

        let payload = vec![9u8; 800];
        host.send(Channel::Video, &payload, 9, true).unwrap();
        let mut got = None;
        for _ in 0..40 {
            host.poll(&mut hin).unwrap();
            client2.poll(&mut c2).unwrap();
            if let Some(Incoming::Datagram { payload: p, .. }) = c2
                .iter()
                .find(|m| matches!(m, Incoming::Datagram { channel: Channel::Video, .. }))
            {
                got = Some(p.clone());
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got.as_deref(), Some(payload.as_slice()));
    }

    #[test]
    fn control_not_blocked_by_reliable_video() {
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
        host.send(Channel::Control, b"caps-a", 0, true).unwrap();
        host.send(Channel::Video, &vec![1u8; 4000], 1, true).unwrap();
        host.send(Channel::Control, b"caps-b", 0, true).unwrap();
        let mut got_a = false;
        let mut got_b = false;
        for _ in 0..50 {
            host.poll(&mut hin).unwrap();
            client.poll(&mut cin).unwrap();
            for m in &cin {
                if let Incoming::Datagram {
                    channel: Channel::Control,
                    payload,
                    ..
                } = m
                {
                    if payload == b"caps-a" {
                        got_a = true;
                    }
                    if payload == b"caps-b" {
                        got_b = true;
                    }
                }
            }
            if got_a && got_b {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(got_a && got_b, "control after reliable keyframe must still be delivered");
    }
}

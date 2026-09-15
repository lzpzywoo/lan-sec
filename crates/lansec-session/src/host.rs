use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use lansec_audio::{OpusRoundtrip, PcmGather};
use lansec_bud::{BudConfig, BudEndpoint, Incoming};
use lansec_capture::CaptureSession;
use lansec_encode::{EncoderConfig, HardwareEncoder};
#[cfg(not(windows))]
use lansec_encode::open_encoder;
use lansec_input::inject;
use lansec_protocol::{
    decode, encode, Caps, Channel, Chroma, ControlMsg, FrameTimes, InputEvent, NegotiatedFormat, VideoAccessUnit,
};
use tracing::{info, warn};

use crate::local_caps;

enum HostCmd {
    Established,
    CapsOffer(Caps),
    CapsAccept(NegotiatedFormat),
    Bye,
}

struct HostStats {
    last: Instant,
    frames: u32,
    skip: u32,
    fresh: u32,
    repeat: u32,
    encode_us: u64,
    encode_max_us: u64,
    send_us: u64,
    send_max_us: u64,
    video_bytes: u64,
}

impl HostStats {
    fn new() -> Self {
        Self {
            last: Instant::now(),
            frames: 0,
            skip: 0,
            fresh: 0,
            repeat: 0,
            encode_us: 0,
            encode_max_us: 0,
            send_us: 0,
            send_max_us: 0,
            video_bytes: 0,
        }
    }

    fn note_encode(&mut self, us: u64) {
        self.encode_us += us;
        self.encode_max_us = self.encode_max_us.max(us);
    }

    fn note_send(&mut self, us: u64) {
        self.send_us += us;
        self.send_max_us = self.send_max_us.max(us);
    }

    fn maybe_print(&mut self, bud: &BudEndpoint, input: &AtomicU32) {
        if self.last.elapsed() < Duration::from_millis(500) {
            return;
        }
        let dt = self.last.elapsed().as_secs_f32().max(0.001);
        let fps = self.frames as f32 / dt;
        let video_mbps = self.video_bytes as f32 * 8.0 / dt / 1_000_000.0;
        let enc_avg = if self.frames > 0 {
            self.encode_us as f32 / self.frames as f32 / 1000.0
        } else {
            0.0
        };
        let send_avg = if self.frames > 0 {
            self.send_us as f32 / self.frames as f32 / 1000.0
        } else {
            0.0
        };
        let inputs = input.swap(0, Ordering::Relaxed);
        let link = bud.snapshot_link();
        eprintln!(
            "host {:.2}s fps={:.1} video={:.1}Mbps udp_tx={:.1}Mbps target={:.1}Mbps encode={:.1}/{:.1}ms send={:.1}/{:.1}ms skip={} fresh={} repeat={} input={:.0}/s rtt={:.1}ms loss={:.2}% wblock={} rel={} hold={} vbuf={}",
            dt,
            fps,
            video_mbps,
            link.send_mbps,
            link.target_mbps,
            enc_avg,
            self.encode_max_us as f32 / 1000.0,
            send_avg,
            self.send_max_us as f32 / 1000.0,
            self.skip,
            self.fresh,
            self.repeat,
            inputs as f32 / dt,
            link.rtt_us as f32 / 1000.0,
            link.loss_ppm as f32 / 10_000.0,
            link.would_block,
            link.reliable_out,
            link.reliable_hold,
            link.video_partial,
        );
        *self = Self::new();
    }
}

pub fn run_host(bind: SocketAddr, pin: String) -> Result<()> {
    let bud = Arc::new(BudEndpoint::bind(BudConfig {
        bind,
        pin,
        is_host: true,
    })?);
    info!(addr = %bud.local_addr()?, "host listening");
    eprintln!(
        "host stats every 0.5s — video=HEVC Mbps  udp_tx=socket Mbps  target=setpoint  encode=avg/max  skip=encode miss  fresh/repeat=SCK  wblock=UDP full"
    );
    let local = local_caps();
    let (tx, rx) = mpsc::channel::<HostCmd>();
    let need_idr = Arc::new(AtomicBool::new(true));
    let bye = Arc::new(AtomicBool::new(false));
    let bitrate = Arc::new(AtomicU32::new(bud.congestion.lock().target_bps));
    let input_count = Arc::new(AtomicU32::new(0));

    {
        let bud_net = Arc::clone(&bud);
        let local_net = local.clone();
        let need_idr_net = Arc::clone(&need_idr);
        let bye_net = Arc::clone(&bye);
        let bitrate_net = Arc::clone(&bitrate);
        let input_net = Arc::clone(&input_count);
        thread::Builder::new()
            .name("lansec-input".into())
            .spawn(move || net_loop(bud_net, local_net, tx, need_idr_net, bye_net, bitrate_net, input_net))
            .expect("input thread");
    }

    let mut established = false;
    let mut format: Option<NegotiatedFormat> = None;
    let mut capture = CaptureSession::open().ok();
    let mut encoder: Option<Box<dyn HardwareEncoder>> = None;
    let mut force_idr = true;
    let mut frame_id = 0u32;
    let mut opus = OpusRoundtrip::new().ok();
    let mut pcm = PcmGather::default();
    let mut stats = HostStats::new();
    let mut last_bps = 0u32;
    let mut next_frame_at = Instant::now();
    const FRAME_DT: Duration = Duration::from_micros(16_667);
    #[cfg(windows)]
    let mut loopback = lansec_audio::wasapi::Loopback::open().ok();

    loop {
        if bye.load(Ordering::Relaxed) {
            return Ok(());
        }
        match rx.try_recv() {
            Ok(HostCmd::Established) => {
                established = true;
                format = None;
                encoder = None;
                force_idr = true;
                last_bps = 0;
                need_idr.store(true, Ordering::Relaxed);
            }
            Ok(HostCmd::CapsOffer(remote)) => {
                match negotiate_with_size(&local, &remote, capture.as_ref()) {
                    Ok(fmt) => {
                        info!(chroma = fmt.chroma_label(), encode = ?fmt.encode, decode = ?fmt.decode, "negotiated");
                        println!("stream format: {}", fmt.chroma_label());
                        if let Ok(bytes) = encode(&ControlMsg::CapsAccept { format: fmt }) {
                            let _ = bud.send(Channel::Control, &bytes, 0, true);
                        }
                        format = Some(fmt);
                        encoder = None;
                        last_bps = 0;
                    }
                    Err(e) => warn!("negotiate: {e}"),
                }
            }
            Ok(HostCmd::CapsAccept(fmt)) => {
                info!(chroma = fmt.chroma_label(), "peer accepted");
                println!("stream format: {}", fmt.chroma_label());
                format = Some(fmt);
                encoder = None;
                last_bps = 0;
            }
            Ok(HostCmd::Bye) => return Ok(()),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Ok(()),
        }
        if need_idr.swap(false, Ordering::AcqRel) {
            force_idr = true;
        }
        if established {
            if encoder.is_none() {
                if let (Some(cap), Some(fmt)) = (capture.as_ref(), format) {
                    let (w, h) = cap.size();
                    let cfg = EncoderConfig {
                        width: w,
                        height: h,
                        bitrate_bps: bitrate.load(Ordering::Relaxed),
                        prefer_444: fmt.chroma == Chroma::Yuv444,
                    };
                    #[cfg(windows)]
                    {
                        encoder = lansec_encode::open_encoder_with_gpu(cap.gpu(), cfg).ok();
                    }
                    #[cfg(not(windows))]
                    {
                        encoder = open_encoder(cfg).ok();
                    }
                    if let Some(enc) = encoder.as_ref() {
                        info!(backend = ?enc.backend(), chroma = ?enc.chroma(), "hardware encoder ready");
                        println!("encode chroma: {:?}", enc.chroma());
                    }
                }
            }
            let now = Instant::now();
            if now >= next_frame_at {
                if let (Some(cap), Some(enc)) = (capture.as_mut(), encoder.as_mut()) {
                    if let Ok(Some(frame)) = cap.next_frame() {
                        if frame.info.fresh {
                            stats.fresh += 1;
                        } else {
                            stats.repeat += 1;
                        }
                        let t_enc = Instant::now();
                        match enc.encode(&frame, force_idr) {
                            Ok(Some(au)) => {
                                force_idr = false;
                                let encode_us = t_enc.elapsed().as_micros() as u64;
                                stats.note_encode(encode_us);
                                let mut times = FrameTimes {
                                    capture_us: frame.info.capture_us,
                                    encode_done_us: frame.info.capture_us + encode_us,
                                    ..Default::default()
                                };
                                times.send_us = times.encode_done_us;
                                let video_bytes = au.annexb.len() as u64;
                                let packet = VideoAccessUnit {
                                    frame_id,
                                    is_keyframe: au.is_keyframe,
                                    width: frame.info.width as u16,
                                    height: frame.info.height as u16,
                                    times,
                                    annexb: au.annexb,
                                };
                                frame_id = frame_id.wrapping_add(1);
                                let t_send = Instant::now();
                                if let Ok(bytes) = encode(&packet) {
                                    let _ = bud.send(Channel::Video, &bytes, packet.frame_id, packet.is_keyframe);
                                }
                                stats.note_send(t_send.elapsed().as_micros() as u64);
                                stats.frames += 1;
                                stats.video_bytes += video_bytes;
                                let bps = bitrate.load(Ordering::Relaxed);
                                if bps != last_bps {
                                    enc.set_bitrate(bps);
                                    last_bps = bps;
                                }
                            }
                            Ok(None) => stats.skip += 1,
                            Err(e) => warn!("encode: {e}"),
                        }
                    }
                }
                next_frame_at += FRAME_DT;
                let caught_up = Instant::now();
                while next_frame_at < caught_up {
                    next_frame_at += FRAME_DT;
                }
            }
            #[cfg(target_os = "macos")]
            if let Some(cap) = capture.as_mut() {
                let samples = cap.next_audio();
                pump_audio(&bud, &mut opus, &mut pcm, samples);
            }
            #[cfg(windows)]
            if let Some(lb) = loopback.as_mut() {
                if let Ok(samples) = lb.read_f32() {
                    pump_audio(&bud, &mut opus, &mut pcm, &samples);
                }
            }
            stats.maybe_print(&bud, &input_count);
            let wait = next_frame_at.checked_duration_since(Instant::now()).unwrap_or(Duration::ZERO);
            if !wait.is_zero() {
                thread::sleep(wait.min(Duration::from_micros(200)));
            }
        } else {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

fn net_loop(
    bud: Arc<BudEndpoint>,
    local: Caps,
    tx: mpsc::Sender<HostCmd>,
    need_idr: Arc<AtomicBool>,
    bye: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
    input_count: Arc<AtomicU32>,
) {
    loop {
        if bye.load(Ordering::Relaxed) {
            return;
        }
        let mut incoming = Vec::new();
        if bud.poll(&mut incoming).is_err() {
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        let idle = incoming.is_empty();
        let mut last_move = None;
        for msg in incoming {
            match msg {
                Incoming::Established { peer } => {
                    info!(%peer, "BUD handshake complete");
                    if let Ok(bytes) = encode(&ControlMsg::CapsOffer(local.clone())) {
                        let _ = bud.send(Channel::Control, &bytes, 0, true);
                    }
                    need_idr.store(true, Ordering::Relaxed);
                    if tx.send(HostCmd::Established).is_err() {
                        return;
                    }
                }
                Incoming::NeedIdr => need_idr.store(true, Ordering::Relaxed),
                Incoming::Datagram {
                    channel: Channel::Control,
                    payload,
                    ..
                } => match decode::<ControlMsg>(&payload) {
                    Ok(ControlMsg::CapsOffer(remote)) => {
                        if tx.send(HostCmd::CapsOffer(remote)).is_err() {
                            return;
                        }
                    }
                    Ok(ControlMsg::CapsAccept { format: fmt }) => {
                        if tx.send(HostCmd::CapsAccept(fmt)).is_err() {
                            return;
                        }
                    }
                    Ok(ControlMsg::RequestIdr) => need_idr.store(true, Ordering::Relaxed),
                    Ok(ControlMsg::Congestion(r)) => {
                        bitrate.store(r.suggested_bitrate_bps, Ordering::Relaxed);
                    }
                    Ok(ControlMsg::Bye) => {
                        bye.store(true, Ordering::Relaxed);
                        let _ = tx.send(HostCmd::Bye);
                        return;
                    }
                    Err(e) => warn!("control decode: {e}"),
                },
                Incoming::Datagram {
                    channel: Channel::Input,
                    payload,
                    ..
                } => {
                    if let Ok(ev) = decode::<InputEvent>(&payload) {
                        if matches!(ev, InputEvent::MouseMoveAbs { .. } | InputEvent::MouseMoveRel { .. }) {
                            last_move = Some(ev);
                        } else {
                            if let Some(mv) = last_move.take() {
                                let _ = inject(&mv);
                                input_count.fetch_add(1, Ordering::Relaxed);
                            }
                            let _ = inject(&ev);
                            input_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Incoming::Datagram { .. } => {}
            }
        }
        if let Some(mv) = last_move {
            let _ = inject(&mv);
            input_count.fetch_add(1, Ordering::Relaxed);
        }
        if idle {
            thread::sleep(Duration::from_micros(250));
        }
    }
}

fn negotiate_with_size(
    local: &Caps,
    remote: &Caps,
    capture: Option<&CaptureSession>,
) -> Result<NegotiatedFormat> {
    let mut fmt = lansec_protocol::negotiate(local, remote).ok_or_else(|| anyhow!("no common codec"))?;
    if let Some(cap) = capture {
        let (w, h) = cap.size();
        fmt.width = w as u16;
        fmt.height = h as u16;
    }
    Ok(fmt)
}

fn pump_audio(bud: &BudEndpoint, opus: &mut Option<OpusRoundtrip>, gather: &mut PcmGather, samples: &[f32]) {
    let Some(rt) = opus.as_mut() else {
        return;
    };
    for frame in gather.push(samples) {
        if let Ok(pkt) = rt.encode_pcm(&frame) {
            if let Ok(bytes) = encode(&pkt) {
                let _ = bud.send(Channel::Audio, &bytes, 0, false);
            }
        }
    }
}

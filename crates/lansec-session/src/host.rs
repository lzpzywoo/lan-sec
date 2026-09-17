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
#[cfg(target_os = "macos")]
use lansec_input::{inject_with_buttons, MouseButtons};
#[cfg(not(target_os = "macos"))]
use lansec_input::inject;
use lansec_protocol::{
    decode, decode_control, encode, Caps, Channel, Chroma, ChromaPref, ControlMsg, EncodeBackend,
    DecodeBackend, FrameTimes, InputEvent, NegotiatedFormat, Platform, VideoAccessUnit,
};
use tracing::{info, warn};

use crate::local_caps_with_pref;

enum HostCmd {
    Established,
    CapsOffer(Caps),
    CapsAccept(NegotiatedFormat),
    Bye,
}

/// Track pressed buttons so we never coalesce moves during a drag (Mac or Windows host).
#[derive(Default)]
struct HeldMouse {
    left: bool,
    right: bool,
    middle: bool,
}

impl HeldMouse {
    fn any_down(&self) -> bool {
        self.left || self.right || self.middle
    }

    fn note(&mut self, ev: &InputEvent) {
        if let InputEvent::MouseButton { button, down } = *ev {
            match button {
                0 => self.left = down,
                1 => self.right = down,
                _ => self.middle = down,
            }
        }
    }
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

pub fn run_host(cfg: crate::SessionConfig) -> Result<()> {
    let mut cfg = cfg;
    cfg.mode = crate::SessionMode::Host;
    cfg.clamp();
    let bind = cfg.bind_addr()?;
    let bud = Arc::new(BudEndpoint::bind(BudConfig {
        bind,
        pin: cfg.pin.clone(),
        is_host: true,
        target_bps: Some(cfg.target_bps()),
        min_bps: Some(cfg.min_bps()),
        max_bps: Some(cfg.max_bps()),
    })?);
    info!(
        addr = %bud.local_addr()?,
        fps = cfg.target_fps,
        target_mbps = cfg.target_mbps,
        min_mbps = cfg.min_mbps,
        max_mbps = cfg.max_mbps,
        chroma = ?cfg.chroma,
        "host listening"
    );
    eprintln!(
        "host stats every 0.5s — video=HEVC Mbps  udp_tx=socket Mbps  target=setpoint  encode=avg/max  skip=encode miss  fresh/repeat=SCK  wblock=UDP full"
    );
    let local = local_caps_with_pref(cfg.chroma);
    let chroma_pref = cfg.chroma;
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
    let mut last_encode = Instant::now();
    let mut last_content_at = Instant::now() - Duration::from_secs(1);
    let target_fps = cfg.target_fps.max(1);
    let frame_interval = Duration::from_nanos(1_000_000_000 / target_fps as u64);
    let motion_boost_bps = cfg.max_bps();
    const MOTION_IDLE: Duration = Duration::from_millis(300);
    // Keep encoding briefly after last content change so CBR/ABR averages do not collapse.
    const MOTION_HOLD: Duration = Duration::from_millis(200);
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
                let mut remote = remote;
                // Older / probe-failed Windows clients sometimes advertise encode only
                // (empty decode). Assume D3D11VA HEVC so Mac→Win can still negotiate.
                if remote.decode.is_empty()
                    && matches!(remote.platform, Platform::Windows | Platform::Unknown)
                {
                    warn!("peer decode caps empty; synthesizing D3D11VA HEVC 4:2:0/4:4:4");
                    remote.decode = vec![
                        lansec_protocol::CodecCap::decode(
                            DecodeBackend::D3d11va,
                            Chroma::Yuv420,
                            3840,
                            2160,
                        ),
                        lansec_protocol::CodecCap::decode(
                            DecodeBackend::D3d11va,
                            Chroma::Yuv444,
                            3840,
                            2160,
                        ),
                    ];
                }
                // Prefer host SessionConfig chroma; Mac→Win always force 420 (Win 444 present is unsafe).
                let pref = if local.platform == Platform::Macos
                    && matches!(remote.platform, Platform::Windows | Platform::Unknown)
                {
                    ChromaPref::Yuv420
                } else {
                    chroma_pref
                };
                match negotiate_with_size(&local, &remote, capture.as_ref(), pref) {
                    Ok(fmt) => {
                        info!(chroma = fmt.chroma_label(), encode = ?fmt.encode, decode = ?fmt.decode, "negotiated");
                        if fmt.chroma == Chroma::Yuv420 {
                            println!(
                                "stream format: {} encode={:?} decode={:?} (4:2:0 — text edges chroma-subsampled)",
                                fmt.chroma_label(),
                                fmt.encode,
                                fmt.decode
                            );
                        } else {
                            println!(
                                "stream format: {} encode={:?} decode={:?}",
                                fmt.chroma_label(),
                                fmt.encode,
                                fmt.decode
                            );
                        }
                        if let Ok(bytes) = encode(&ControlMsg::CapsAccept { format: fmt }) {
                            let _ = bud.send(Channel::Control, &bytes, 0, true);
                        }
                        format = Some(fmt);
                        encoder = None;
                        last_bps = 0;
                    }
                    Err(e) => {
                        warn!(
                            %e,
                            local_encode = local.encode.len(),
                            remote_decode = remote.decode.len(),
                            remote_encode = remote.encode.len(),
                            remote_platform = ?remote.platform,
                            "negotiate failed; peer caps dump follows"
                        );
                        for (i, c) in remote.decode.iter().enumerate() {
                            warn!(i, chroma = ?c.chroma, decode = ?c.decode, "peer decode cap");
                        }
                        for (i, c) in remote.encode.iter().enumerate() {
                            warn!(i, chroma = ?c.chroma, encode = ?c.encode, "peer encode cap");
                        }
                        // Mac host → Windows client: force HEVC 4:2:0 so a white screen from
                        // empty/corrupt peer decode ads still gets a stream (client opens from CapsAccept).
                        if local.platform == Platform::Macos
                            && matches!(remote.platform, Platform::Windows | Platform::Unknown)
                            && local.encode.iter().any(|c| {
                                c.chroma == Chroma::Yuv420 && c.encode == Some(EncodeBackend::VideoToolbox)
                            })
                        {
                            let (w, h) = capture
                                .as_ref()
                                .map(|c| c.size())
                                .unwrap_or((1920, 1080));
                            let fmt = NegotiatedFormat {
                                codec: lansec_protocol::Codec::Hevc,
                                chroma: Chroma::Yuv420,
                                bit_depth: 8,
                                encode: EncodeBackend::VideoToolbox,
                                decode: DecodeBackend::D3d11va,
                                width: w as u16,
                                height: h as u16,
                            };
                            warn!(
                                chroma = fmt.chroma_label(),
                                "falling back to Mac→Win HEVC 4:2:0 CapsAccept"
                            );
                            println!(
                                "stream format: {} encode={:?} decode={:?} (fallback)",
                                fmt.chroma_label(),
                                fmt.encode,
                                fmt.decode
                            );
                            if let Ok(bytes) = encode(&ControlMsg::CapsAccept { format: fmt }) {
                                let _ = bud.send(Channel::Control, &bytes, 0, true);
                            }
                            format = Some(fmt);
                            encoder = None;
                            last_bps = 0;
                        }
                    }
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
            let mut encoded = false;
            if let (Some(cap), Some(enc)) = (capture.as_mut(), encoder.as_mut()) {
                if let Ok(Some(frame)) = cap.next_frame() {
                    if frame.info.fresh {
                        last_content_at = Instant::now();
                    }
                    // Encode on content change, IDR, or briefly after motion so bitrate
                    // does not collapse between SCK idle frames during a drag.
                    let in_motion = last_content_at.elapsed() < MOTION_HOLD;
                    let due = frame.info.fresh || force_idr || in_motion;
                    if !due {
                        if last_encode.elapsed() >= frame_interval {
                            stats.repeat += 1;
                            last_encode += frame_interval;
                            if last_encode.elapsed() > frame_interval {
                                last_encode = Instant::now();
                            }
                        }
                    } else {
                        if frame.info.fresh {
                            stats.fresh += 1;
                        } else {
                            stats.repeat += 1;
                        }
                        let t_enc = Instant::now();
                        match enc.encode(&frame, force_idr) {
                            Ok(Some(au)) => {
                                force_idr = false;
                                encoded = true;
                                last_encode = Instant::now();
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
                                let cong_bps = bitrate.load(Ordering::Relaxed);
                                let bps = if last_content_at.elapsed() < MOTION_IDLE {
                                    cong_bps.max(motion_boost_bps)
                                } else {
                                    cong_bps
                                };
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
            if !encoded {
                thread::sleep(Duration::from_micros(200));
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
    #[cfg(target_os = "macos")]
    let mut mouse_buttons = MouseButtons::default();
    let mut held = HeldMouse::default();
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
        // While a button is held, every move must be injected (Mac LeftMouseDragged /
        // Win continuous moves). Coalescing made window drags "teleport" on mouse-up.
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
                } => match decode_control(&payload) {
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
                            if !held.any_down() {
                                last_move = Some(ev);
                            } else {
                                #[cfg(target_os = "macos")]
                                let _ = inject_with_buttons(&ev, &mut mouse_buttons);
                                #[cfg(not(target_os = "macos"))]
                                let _ = inject(&ev);
                                input_count.fetch_add(1, Ordering::Relaxed);
                            }
                        } else {
                            if let Some(mv) = last_move.take() {
                                #[cfg(target_os = "macos")]
                                let _ = inject_with_buttons(&mv, &mut mouse_buttons);
                                #[cfg(not(target_os = "macos"))]
                                let _ = inject(&mv);
                                input_count.fetch_add(1, Ordering::Relaxed);
                            }
                            held.note(&ev);
                            #[cfg(target_os = "macos")]
                            let _ = inject_with_buttons(&ev, &mut mouse_buttons);
                            #[cfg(not(target_os = "macos"))]
                            let _ = inject(&ev);
                            input_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Incoming::Datagram { .. } => {}
            }
        }
        if let Some(mv) = last_move {
            #[cfg(target_os = "macos")]
            let _ = inject_with_buttons(&mv, &mut mouse_buttons);
            #[cfg(not(target_os = "macos"))]
            let _ = inject(&mv);
            input_count.fetch_add(1, Ordering::Relaxed);
        }
        if idle {
            thread::sleep(Duration::from_micros(50));
        }
    }
}

fn negotiate_with_size(
    local: &Caps,
    remote: &Caps,
    capture: Option<&CaptureSession>,
    host_pref: ChromaPref,
) -> Result<NegotiatedFormat> {
    let mut fmt = lansec_protocol::negotiate_with_pref(local, remote, host_pref)
        .ok_or_else(|| anyhow!("no common codec"))?;
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

use std::net::SocketAddr;
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

pub fn run_host(bind: SocketAddr, pin: String) -> Result<()> {
    let bud = BudEndpoint::bind(BudConfig {
        bind,
        pin,
        is_host: true,
    })?;
    info!(addr = %bud.local_addr()?, "host listening");
    let local = local_caps();
    let mut established = false;
    let mut format: Option<NegotiatedFormat> = None;
    let mut capture = CaptureSession::open().ok();
    let mut encoder: Option<Box<dyn HardwareEncoder>> = None;
    let mut force_idr = true;
    let mut frame_id = 0u32;
    let mut opus = OpusRoundtrip::new().ok();
    let mut pcm = PcmGather::default();
    #[cfg(windows)]
    let mut loopback = lansec_audio::wasapi::Loopback::open().ok();
    loop {
        if !drain_incoming(
            &bud,
            &local,
            capture.as_ref(),
            &mut encoder,
            &mut established,
            &mut format,
            &mut force_idr,
        )? {
            return Ok(());
        }
        if established {
            if encoder.is_none() {
                if let (Some(cap), Some(fmt)) = (capture.as_ref(), format) {
                    let (w, h) = cap.size();
                    let cfg = EncoderConfig {
                        width: w,
                        height: h,
                        bitrate_bps: bud.congestion.lock().target_bps,
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
            if let (Some(cap), Some(enc)) = (capture.as_mut(), encoder.as_mut()) {
                if let Ok(Some(frame)) = cap.next_frame() {
                    let t_enc = Instant::now();
                    match enc.encode(&frame, force_idr) {
                        Ok(Some(au)) => {
                            force_idr = false;
                            let mut times = FrameTimes {
                                capture_us: frame.info.capture_us,
                                encode_done_us: frame.info.capture_us + t_enc.elapsed().as_micros() as u64,
                                ..Default::default()
                            };
                            times.send_us = times.encode_done_us;
                            let packet = VideoAccessUnit {
                                frame_id,
                                is_keyframe: au.is_keyframe,
                                width: frame.info.width as u16,
                                height: frame.info.height as u16,
                                times,
                                annexb: au.annexb,
                            };
                            frame_id = frame_id.wrapping_add(1);
                            let bytes = encode(&packet)?;
                            bud.send(Channel::Video, &bytes, packet.frame_id, packet.is_keyframe)?;
                            enc.set_bitrate(bud.congestion.lock().target_bps);
                        }
                        Ok(None) => {}
                        Err(e) => warn!("encode: {e}"),
                    }
                }
            }
            #[cfg(target_os = "macos")]
            if let Some(cap) = capture.as_mut() {
                pump_audio(&bud, &mut opus, &mut pcm, &cap.next_audio());
            }
            #[cfg(windows)]
            if let Some(lb) = loopback.as_mut() {
                if let Ok(samples) = lb.read_f32() {
                    pump_audio(&bud, &mut opus, &mut pcm, &samples);
                }
            }
        }
        // Input must not sit behind encode: poll again before the 1ms sleep.
        if !drain_incoming(
            &bud,
            &local,
            capture.as_ref(),
            &mut encoder,
            &mut established,
            &mut format,
            &mut force_idr,
        )? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn drain_incoming(
    bud: &BudEndpoint,
    local: &Caps,
    capture: Option<&CaptureSession>,
    encoder: &mut Option<Box<dyn HardwareEncoder>>,
    established: &mut bool,
    format: &mut Option<NegotiatedFormat>,
    force_idr: &mut bool,
) -> Result<bool> {
    let mut incoming = Vec::new();
    bud.poll(&mut incoming)?;
    for msg in incoming {
        match msg {
            Incoming::Established { peer } => {
                info!(%peer, "BUD handshake complete");
                *established = true;
                *format = None;
                *encoder = None;
                *force_idr = true;
                let bytes = encode(&ControlMsg::CapsOffer(local.clone()))?;
                bud.send(Channel::Control, &bytes, 0, true)?;
            }
            Incoming::NeedIdr => *force_idr = true,
            Incoming::Datagram {
                channel: Channel::Control,
                payload,
                ..
            } => match decode::<ControlMsg>(&payload)? {
                ControlMsg::CapsOffer(remote) => {
                    let fmt = negotiate_with_size(local, &remote, capture)?;
                    info!(chroma = fmt.chroma_label(), encode = ?fmt.encode, decode = ?fmt.decode, "negotiated");
                    println!("stream format: {}", fmt.chroma_label());
                    let bytes = encode(&ControlMsg::CapsAccept { format: fmt })?;
                    bud.send(Channel::Control, &bytes, 0, true)?;
                    *format = Some(fmt);
                }
                ControlMsg::CapsAccept { format: fmt } => {
                    info!(chroma = fmt.chroma_label(), "peer accepted");
                    println!("stream format: {}", fmt.chroma_label());
                    *format = Some(fmt);
                }
                ControlMsg::RequestIdr => *force_idr = true,
                ControlMsg::Congestion(r) => {
                    if let Some(enc) = encoder.as_mut() {
                        enc.set_bitrate(r.suggested_bitrate_bps);
                        info!(bps = r.suggested_bitrate_bps, rtt_us = r.rtt_us, loss_ppm = r.loss_ppm, "bitrate from client");
                    }
                }
                ControlMsg::Bye => return Ok(false),
            },
            Incoming::Datagram {
                channel: Channel::Input,
                payload,
                ..
            } => {
                if let Ok(ev) = decode::<InputEvent>(&payload) {
                    let _ = inject(&ev);
                }
            }
            Incoming::Datagram { .. } => {}
        }
    }
    Ok(true)
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

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::Result;
use lansec_audio::play::Player;
use lansec_audio::OpusRoundtrip;
use lansec_bud::{BudConfig, BudEndpoint, Incoming};
use lansec_decode::{open_decoder, HardwareDecoder};
use lansec_present::Presenter;
use lansec_protocol::{
    decode, decode_control, encode, Caps, Channel, ControlMsg, InputEvent, SessionClock,
    VideoAccessUnit,
};
use tracing::{info, warn};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};
#[cfg(windows)]
use winit::platform::windows::WindowAttributesExtWindows;

use crate::keys;
use crate::local_caps_with_pref;
use crate::SessionConfig;

pub fn run_client(cfg: SessionConfig) -> Result<()> {
    let mut cfg = cfg;
    cfg.mode = crate::SessionMode::Client;
    cfg.clamp();
    let connect = cfg.connect_addr()?;
    info!(
        %connect,
        fps = cfg.target_fps,
        chroma = ?cfg.chroma,
        "client connecting"
    );
    eprintln!(
        "client stats every 0.5s — video=HEVC Mbps  udp_rx=socket Mbps  target=setpoint  gap=max frame interval  wblock=UDP full"
    );
    let local = local_caps_with_pref(cfg.chroma);
    #[cfg(windows)]
    let gpu = lansec_capture::GpuContext::new().ok();
    let player = Player::start().ok();
    let bud = BudEndpoint::bind(BudConfig {
        bind: "0.0.0.0:0".parse().unwrap(),
        pin: cfg.pin.clone(),
        is_host: false,
        target_bps: Some(cfg.target_bps()),
        min_bps: Some(cfg.min_bps()),
        max_bps: Some(cfg.max_bps()),
    })?;
    bud.connect(connect)?;
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let present_hz = cfg.target_fps.max(1) as f32;
    let mut app = ClientApp {
        bud,
        local,
        decoder: None,
        presenter: Presenter::new(present_hz),
        opus: OpusRoundtrip::new().ok(),
        player,
        window: None,
        #[cfg(windows)]
        gpu,
        #[cfg(windows)]
        swap: None,
        #[cfg(target_os = "macos")]
        metal: None,
        host_w: 1920,
        host_h: 1080,
        last_cong: Instant::now(),
        last_stats: Instant::now(),
        clock: SessionClock::new(),
        video_frames: 0,
        video_without_decoder: 0,
        decode_idle: 0,
        await_keyframe: false,
        decode_errors: 0,
        win_frames: 0,
        win_video_bytes: 0,
        win_decode_us: 0,
        win_decode_max_us: 0,
        win_present_us: 0,
        win_present_max_us: 0,
        win_pump_max_us: 0,
        win_gap_max_us: 0,
        last_video_at: None,
        last_idr_req: None,
        video_q: VecDeque::new(),
        last_presented: None,
        last_represent: Instant::now(),
        represent_interval: Duration::from_nanos(1_000_000_000 / cfg.target_fps.max(1) as u64),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct ClientApp {
    bud: BudEndpoint,
    local: Caps,
    decoder: Option<Box<dyn HardwareDecoder>>,
    presenter: Presenter,
    opus: Option<OpusRoundtrip>,
    player: Option<Player>,
    window: Option<Window>,
    #[cfg(windows)]
    gpu: Option<lansec_capture::GpuContext>,
    #[cfg(windows)]
    swap: Option<lansec_present::windows_swapchain::Swapchain>,
    #[cfg(target_os = "macos")]
    metal: Option<lansec_present::macos_metal::MetalLayer>,
    host_w: u16,
    host_h: u16,
    last_cong: Instant,
    last_stats: Instant,
    clock: SessionClock,
    video_frames: u64,
    video_without_decoder: u64,
    decode_idle: u64,
    /// After opening a decoder, ignore P-frames until the first IDR arrives.
    await_keyframe: bool,
    decode_errors: u64,
    win_frames: u32,
    win_video_bytes: u64,
    win_decode_us: u64,
    win_decode_max_us: u64,
    win_present_us: u64,
    win_present_max_us: u64,
    win_pump_max_us: u64,
    win_gap_max_us: u64,
    last_video_at: Option<Instant>,
    last_idr_req: Option<Instant>,
    video_q: VecDeque<VideoAccessUnit>,
    last_presented: Option<lansec_decode::DecodedFrame>,
    last_represent: Instant,
    represent_interval: Duration,
}

impl ClientApp {
    fn send_input(&self, ev: &InputEvent) {
        if let Ok(bytes) = encode(ev) {
            // Absolute mouse must not use the reliable window. A single lost
            // move would hold every later click until retransmission — that is
            // the main reason LAN input feels slower than Parsec.
            let reliable = !matches!(
                ev,
                InputEvent::MouseMoveAbs { .. } | InputEvent::MouseMoveRel { .. }
            );
            let _ = self.bud.send_with(Channel::Input, &bytes, 0, false, reliable);
        }
    }

    fn request_idr(&mut self) {
        if self.last_idr_req.is_some_and(|t| t.elapsed() < Duration::from_millis(250)) {
            return;
        }
        self.last_idr_req = Some(Instant::now());
        if let Ok(bytes) = encode(&ControlMsg::RequestIdr) {
            let _ = self.bud.send(Channel::Control, &bytes, 0, true);
        }
    }

    fn pump(&mut self) {
        let t0 = Instant::now();
        let mut incoming = Vec::new();
        if self.bud.poll(&mut incoming).is_err() {
            self.win_pump_max_us = self.win_pump_max_us.max(t0.elapsed().as_micros() as u64);
            return;
        }
        for msg in incoming {
            match msg {
                Incoming::Established { peer } => {
                    info!(%peer, "BUD handshake complete");
                    if let Ok(bytes) = encode(&ControlMsg::CapsOffer(self.local.clone())) {
                        let _ = self.bud.send(Channel::Control, &bytes, 0, true);
                    }
                }
                Incoming::NeedIdr => self.request_idr(),
                Incoming::Datagram {
                    channel: Channel::Control,
                    payload,
                    ..
                } => match decode_control(&payload) {
                    Ok(ControlMsg::CapsOffer(remote)) => {
                        info!(
                            encode = remote.encode.len(),
                            decode = remote.decode.len(),
                            "host caps received; waiting for CapsAccept"
                        );
                    }
                    Ok(ControlMsg::CapsAccept { format: fmt }) => {
                        info!(chroma = fmt.chroma_label(), width = fmt.width, height = fmt.height, "negotiated");
                        if fmt.chroma == lansec_protocol::Chroma::Yuv420 {
                            println!(
                                "stream format: {} (4:2:0 — text edges chroma-subsampled)",
                                fmt.chroma_label()
                            );
                        } else {
                            println!("stream format: {}", fmt.chroma_label());
                        }
                        self.host_w = fmt.width.max(1);
                        self.host_h = fmt.height.max(1);
                        self.open_decoder(fmt.chroma, fmt.width as u32, fmt.height as u32);
                        if let Ok(bytes) = encode(&ControlMsg::RequestIdr) {
                            let _ = self.bud.send(Channel::Control, &bytes, 0, true);
                        }
                    }
                    Ok(ControlMsg::Bye) => {}
                    Ok(_) => {}
                    Err(e) => warn!("control decode: {e}"),
                },
                Incoming::Datagram {
                    channel: Channel::Video,
                    payload,
                    ..
                } => {
                    if let Ok(au) = decode::<VideoAccessUnit>(&payload) {
                        self.video_q.push_back(au);
                    }
                }
                Incoming::Datagram {
                    channel: Channel::Audio,
                    payload,
                    ..
                } => {
                    if let (Ok(pkt), Some(rt)) = (decode::<lansec_protocol::AudioPacket>(&payload), self.opus.as_mut()) {
                        if let Ok(pcm) = rt.decode_opus(&pkt) {
                            if let Some(p) = self.player.as_ref() {
                                p.push(&pcm);
                            }
                        }
                    }
                }
                Incoming::Datagram { .. } => {}
            }
        }
        const VIDEO_Q_MAX: usize = 3;
        if self.video_q.len() > VIDEO_Q_MAX {
            if let Some(i) = self.video_q.iter().rposition(|au| au.is_keyframe) {
                self.video_q.drain(0..i);
            } else {
                self.video_q.clear();
                self.request_idr();
            }
        }
        if self.video_q.len() > 1 {
            while self.video_q.len() > 1 {
                self.video_q.pop_front();
            }
        }
        if let Some(mut au) = self.video_q.pop_back() {
            self.on_video(&mut au);
        }
        self.win_pump_max_us = self.win_pump_max_us.max(t0.elapsed().as_micros() as u64);
    }

    fn open_decoder(&mut self, chroma: lansec_protocol::Chroma, width: u32, height: u32) {
        let w = width.max(1);
        let h = height.max(1);
        #[cfg(windows)]
        {
            self.decoder = None;
            if let Some(gpu) = self.gpu.as_ref() {
                match lansec_decode::open_decoder_with_gpu(gpu, chroma, w, h) {
                    Ok(dec) => self.decoder = Some(dec),
                    Err(e) => warn!(%e, ?chroma, w, h, "open_decoder_with_gpu failed"),
                }
            }
            if self.decoder.is_none() {
                match open_decoder(chroma, w, h) {
                    Ok(dec) => self.decoder = Some(dec),
                    Err(e) => warn!(%e, ?chroma, w, h, "open_decoder failed"),
                }
            }
            // CapsAccept may ask for 444 that Intel cannot present; always keep a 420 path.
            if self.decoder.is_none() && chroma != lansec_protocol::Chroma::Yuv420 {
                warn!(?chroma, "falling back to HEVC 4:2:0 decoder");
                if let Some(gpu) = self.gpu.as_ref() {
                    self.decoder =
                        lansec_decode::open_decoder_with_gpu(gpu, lansec_protocol::Chroma::Yuv420, w, h)
                            .ok();
                }
                if self.decoder.is_none() {
                    self.decoder = open_decoder(lansec_protocol::Chroma::Yuv420, w, h).ok();
                }
            }
        }
        #[cfg(not(windows))]
        {
            match open_decoder(chroma, w, h) {
                Ok(dec) => self.decoder = Some(dec),
                Err(e) => {
                    warn!(%e, ?chroma, w, h, "open_decoder failed");
                    self.decoder = None;
                }
            }
        }
        if let Some(dec) = self.decoder.as_ref() {
            info!(backend = ?dec.backend(), ?chroma, width = w, height = h, "hardware decoder ready");
            self.await_keyframe = true;
            self.request_idr();
        } else {
            warn!(?chroma, width = w, height = h, "hardware decoder unavailable");
        }
        self.invalidate_present_targets();
    }

    fn invalidate_present_targets(&mut self) {
        #[cfg(windows)]
        {
            self.swap = None;
        }
        #[cfg(target_os = "macos")]
        {
            self.metal = None;
        }
    }

    fn on_video(&mut self, au: &mut VideoAccessUnit) {
        if au.width > 0 {
            self.host_w = au.width;
            self.host_h = au.height;
        }
        let recv = self.clock.now_us();
        let rtt = self.bud.congestion.lock().stats().rtt_us as u64;
        if au.times.send_us > 0 {
            let mapped_send = recv.saturating_sub(rtt / 2);
            let offset = mapped_send as i64 - au.times.send_us as i64;
            au.times.capture_us = (au.times.capture_us as i64 + offset).max(0) as u64;
            au.times.encode_done_us = (au.times.encode_done_us as i64 + offset).max(0) as u64;
            au.times.send_us = mapped_send;
        }
        let now = Instant::now();
        if let Some(prev) = self.last_video_at {
            self.win_gap_max_us = self.win_gap_max_us.max(prev.elapsed().as_micros() as u64);
        }
        self.last_video_at = Some(now);
        self.win_frames += 1;
        self.win_video_bytes += au.annexb.len() as u64;
        au.times.recv_us = recv;
        if self.decoder.is_none() {
            self.video_without_decoder += 1;
            if self.video_without_decoder == 1 || self.video_without_decoder % 120 == 0 {
                warn!(
                    n = self.video_without_decoder,
                    w = au.width,
                    h = au.height,
                    key = au.is_keyframe,
                    "video arrived before decoder; trying HEVC 4:2:0 open"
                );
            }
            // Last resort for Mac→Win white screen: CapsAccept missing/failed open.
            // Prefer 420 only — blind 444 was the green/white path on Intel.
            if au.is_keyframe && au.width > 0 {
                self.open_decoder(
                    lansec_protocol::Chroma::Yuv420,
                    au.width as u32,
                    au.height as u32,
                );
                self.request_idr();
            } else if au.is_keyframe {
                self.request_idr();
            }
            if self.decoder.is_none() {
                return;
            }
        }
        if self.await_keyframe {
            if !au.is_keyframe {
                self.request_idr();
                return;
            }
            self.await_keyframe = false;
            info!("first IDR after decoder open; starting decode");
        }
        if let Some(dec) = self.decoder.as_mut() {
            let t_dec = Instant::now();
            match dec.decode(&au.annexb, au.is_keyframe) {
                Ok(Some(frame)) => {
                    let decode_us = t_dec.elapsed().as_micros() as u64;
                    self.win_decode_us += decode_us;
                    self.win_decode_max_us = self.win_decode_max_us.max(decode_us);
                    self.decode_idle = 0;
                    self.decode_errors = 0;
                    au.times.decode_done_us = self.clock.now_us();
                    self.presenter.submit(frame, au.times);
                    if let Some(frame) = self.presenter.take() {
                        let t_present = Instant::now();
                        self.present_frame(frame);
                        let present_us = t_present.elapsed().as_micros() as u64;
                        self.win_present_us += present_us;
                        self.win_present_max_us = self.win_present_max_us.max(present_us);
                    }
                }
                Ok(None) => {
                    self.decode_idle += 1;
                    if self.decode_idle == 1 || self.decode_idle % 120 == 0 {
                        warn!(
                            n = self.decode_idle,
                            key = au.is_keyframe,
                            bytes = au.annexb.len(),
                            "decoder produced no frame"
                        );
                    }
                    if self.decode_idle >= 2 {
                        self.request_idr();
                    }
                }
                Err(e) => {
                    self.decode_errors += 1;
                    if self.decode_errors == 1 || self.decode_errors % 60 == 0 {
                        warn!(n = self.decode_errors, "decode: {e}");
                    }
                    if self.decode_errors == 1 || self.decode_errors % 30 == 0 {
                        self.await_keyframe = true;
                        self.request_idr();
                    }
                }
            }
        }
        self.video_frames += 1;
        if self.last_cong.elapsed().as_millis() >= 100 {
            self.last_cong = Instant::now();
            if let Ok(report) = encode(&ControlMsg::Congestion(self.bud.congestion.lock().report())) {
                let _ = self.bud.send(Channel::Control, &report, 0, true);
            }
        }
    }

    fn maybe_log_stats(&mut self) {
        if self.last_stats.elapsed() < Duration::from_millis(500) {
            return;
        }
        let dt = self.last_stats.elapsed().as_secs_f32().max(0.001);
        let fps = self.win_frames as f32 / dt;
        let video_mbps = self.win_video_bytes as f32 * 8.0 / dt / 1_000_000.0;
        let dec_avg = if self.win_frames > 0 {
            self.win_decode_us as f32 / self.win_frames as f32 / 1000.0
        } else {
            0.0
        };
        let present_avg = if self.win_frames > 0 {
            self.win_present_us as f32 / self.win_frames as f32 / 1000.0
        } else {
            0.0
        };
        let gap_ms = if self.win_frames == 0 {
            self.last_video_at
                .map(|t| t.elapsed().as_secs_f32() * 1000.0)
                .unwrap_or(0.0)
        } else {
            self.win_gap_max_us as f32 / 1000.0
        };
        let t = self.presenter.last_times();
        let link = self.bud.snapshot_link();
        eprintln!(
            "client {:.2}s fps={:.1} video={:.1}Mbps udp_rx={:.1}Mbps target={:.1}Mbps enc={:.1}ms net={:.1}ms decode={:.1}/{:.1}ms present={:.1}/{:.1}ms glass={:.1}ms gap={:.0}ms pump={:.1}ms drop={} rtt={:.1}ms loss={:.2}% wblock={} rel={} vbuf={}",
            dt,
            fps,
            video_mbps,
            link.recv_mbps,
            link.target_mbps,
            t.capture_to_encode_ms(),
            t.net_ms(),
            dec_avg,
            self.win_decode_max_us as f32 / 1000.0,
            present_avg,
            self.win_present_max_us as f32 / 1000.0,
            t.glass_ms(),
            gap_ms,
            self.win_pump_max_us as f32 / 1000.0,
            self.presenter.stats().dropped,
            link.rtt_us as f32 / 1000.0,
            link.loss_ppm as f32 / 10_000.0,
            link.would_block,
            link.reliable_out,
            link.video_partial,
        );
        self.last_stats = Instant::now();
        self.win_frames = 0;
        self.win_video_bytes = 0;
        self.win_decode_us = 0;
        self.win_decode_max_us = 0;
        self.win_present_us = 0;
        self.win_present_max_us = 0;
        self.win_pump_max_us = 0;
        self.win_gap_max_us = 0;
    }

    fn present_frame_ref(&self, frame: &lansec_decode::DecodedFrame) {
        #[cfg(windows)]
        if let (Some(swap), Some(tex)) = (self.swap.as_ref(), frame.d3d11_texture()) {
            if let Err(e) = swap.blit_and_present(tex) {
                warn!("present: {e}");
            }
        }
        #[cfg(target_os = "macos")]
        if let (Some(metal), Some(pb)) = (self.metal.as_ref(), frame.cv_pixel_buffer()) {
            if let Err(e) = metal.present_pixel_buffer(pb) {
                warn!("present: {e}");
            }
        }
    }

    fn present_frame(&mut self, frame: lansec_decode::DecodedFrame) {
        self.present_frame_ref(&frame);
        self.last_presented = Some(frame);
    }

    fn represent_last_if_due(&mut self) {
        if self.last_presented.is_none() || self.last_represent.elapsed() < self.represent_interval {
            return;
        }
        self.last_represent = Instant::now();
        if let Some(frame) = self.last_presented.as_ref() {
            self.present_frame_ref(frame);
        }
    }

    fn ensure_present_targets(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        #[cfg(windows)]
        if self.swap.as_ref().is_some_and(|s| s.width != self.host_w.max(1) as u32 || s.height != self.host_h.max(1) as u32)
        {
            self.swap = None;
        }
        #[cfg(windows)]
        if self.swap.is_none() {
            if let Some(gpu) = self.gpu.as_ref() {
                if let Ok(hwnd) = lansec_present::windows_swapchain::Swapchain::hwnd_from_winit(window) {
                    match lansec_present::windows_swapchain::Swapchain::from_hwnd(
                        &gpu.device,
                        gpu.context.clone(),
                        hwnd,
                        self.host_w.max(1) as u32,
                        self.host_h.max(1) as u32,
                    ) {
                        Ok(s) => self.swap = Some(s),
                        Err(e) => warn!("swapchain: {e}"),
                    }
                }
            }
        }
        #[cfg(target_os = "macos")]
        if self.metal.is_none() {
            match lansec_present::macos_metal::MetalLayer::from_winit(
                window,
                self.host_w.max(1) as u32,
                self.host_h.max(1) as u32,
            ) {
                Ok(m) => self.metal = Some(m),
                Err(e) => warn!("metal: {e}"),
            }
        }
    }
}

impl ApplicationHandler for ClientApp {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, _cause: StartCause) {
        event_loop.set_control_flow(ControlFlow::Poll);
        // Do not decode/present here: winit delivers window events after
        // new_events. Pumping video first adds a full encode/decode stall
        // in front of every mouse packet.
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let mut attrs = Window::default_attributes()
                .with_title("lansec")
                .with_inner_size(winit::dpi::PhysicalSize::new(self.host_w.max(640), self.host_h.max(360)));
            // MF/WASAPI already called CoInitializeEx(COINIT_MULTITHREADED) on this thread.
            // winit's default drag-and-drop path calls OleInitialize (STA) and panics with
            // RPC_E_CHANGED_MODE. We do not need file drop on the viewer.
            #[cfg(windows)]
            {
                attrs = attrs.with_drag_and_drop(false);
            }
            match event_loop.create_window(attrs) {
                Ok(w) => {
                    #[cfg(windows)]
                    if let Ok(hwnd) = lansec_present::windows_swapchain::Swapchain::hwnd_from_winit(&w) {
                        lansec_present::windows_swapchain::Swapchain::exclude_from_capture(hwnd);
                    }
                    self.window = Some(w);
                    if let Some(win) = self.window.as_ref() {
                        // Local cursor is the interactive one; Mac capture excludes the host cursor.
                        win.set_cursor_visible(true);
                    }
                }
                Err(e) => warn!("window: {e}"),
            }
            self.ensure_present_targets();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::CursorMoved { position, .. } => {
                let (ww, wh) = self
                    .window
                    .as_ref()
                    .map(|w| {
                        let s = w.inner_size();
                        (s.width.max(1) as f64, s.height.max(1) as f64)
                    })
                    .unwrap_or((self.host_w.max(1) as f64, self.host_h.max(1) as f64));
                let x = (position.x / ww * self.host_w as f64)
                    .round()
                    .clamp(0.0, u16::MAX as f64) as u16;
                let y = (position.y / wh * self.host_h as f64)
                    .round()
                    .clamp(0.0, u16::MAX as f64) as u16;
                self.send_input(&InputEvent::MouseMoveAbs {
                    x,
                    y,
                    host_w: self.host_w,
                    host_h: self.host_h,
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.send_input(&keys::mouse_button(button, state == winit::event::ElementState::Pressed));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(ev) = keys::mouse_wheel(delta) {
                    self.send_input(&ev);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(ev) = keys::key(event.physical_key, event.state) {
                    if matches!(event.physical_key, PhysicalKey::Code(_)) {
                        self.send_input(&ev);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.ensure_present_targets();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Poll);
        self.ensure_present_targets();
        self.pump();
        self.represent_last_if_due();
        self.maybe_log_stats();
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }
}

use std::net::SocketAddr;
use std::time::Instant;

use anyhow::Result;
use lansec_audio::play::Player;
use lansec_audio::OpusRoundtrip;
use lansec_bud::{BudConfig, BudEndpoint, Incoming};
use lansec_decode::{open_decoder, HardwareDecoder};
use lansec_present::Presenter;
use lansec_protocol::{
    decode, encode, Caps, Channel, ControlMsg, InputEvent, SessionClock, VideoAccessUnit,
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
use crate::local_caps;

pub fn run_client(connect: SocketAddr, pin: String) -> Result<()> {
    info!(%connect, "client connecting");
    let local = local_caps();
    #[cfg(windows)]
    let gpu = lansec_capture::GpuContext::new().ok();
    let player = Player::start().ok();
    let bud = BudEndpoint::bind(BudConfig {
        bind: "0.0.0.0:0".parse().unwrap(),
        pin,
        is_host: false,
    })?;
    bud.connect(connect)?;
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = ClientApp {
        bud,
        local,
        decoder: None,
        presenter: Presenter::new(60.0),
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
        clock: SessionClock::new(),
        video_frames: 0,
        video_without_decoder: 0,
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
    clock: SessionClock,
    video_frames: u64,
    video_without_decoder: u64,
}

impl ClientApp {
    fn send_input(&self, ev: &InputEvent) {
        if let Ok(bytes) = encode(ev) {
            let _ = self.bud.send(Channel::Input, &bytes, 0, false);
        }
    }

    fn pump(&mut self) {
        let mut incoming = Vec::new();
        if self.bud.poll(&mut incoming).is_err() {
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
                Incoming::NeedIdr => {
                    if let Ok(bytes) = encode(&ControlMsg::RequestIdr) {
                        let _ = self.bud.send(Channel::Control, &bytes, 0, true);
                    }
                }
                Incoming::Datagram {
                    channel: Channel::Control,
                    payload,
                    ..
                } => match decode::<ControlMsg>(&payload) {
                    Ok(ControlMsg::CapsOffer(remote)) => {
                        info!(
                            encode = remote.encode.len(),
                            decode = remote.decode.len(),
                            "host caps received; waiting for CapsAccept"
                        );
                    }
                    Ok(ControlMsg::CapsAccept { format: fmt }) => {
                        info!(chroma = fmt.chroma_label(), width = fmt.width, height = fmt.height, "negotiated");
                        println!("stream format: {}", fmt.chroma_label());
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
                    if let Ok(mut au) = decode::<VideoAccessUnit>(&payload) {
                        self.on_video(&mut au);
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
    }

    fn open_decoder(&mut self, chroma: lansec_protocol::Chroma, width: u32, height: u32) {
        #[cfg(windows)]
        {
            self.decoder = if let Some(gpu) = self.gpu.as_ref() {
                lansec_decode::open_decoder_with_gpu(gpu, chroma, width.max(1), height.max(1)).ok()
            } else {
                open_decoder(chroma, width.max(1), height.max(1)).ok()
            };
        }
        #[cfg(not(windows))]
        {
            self.decoder = open_decoder(chroma, width.max(1), height.max(1)).ok();
        }
        if let Some(dec) = self.decoder.as_ref() {
            info!(backend = ?dec.backend(), width, height, "hardware decoder ready");
        } else {
            warn!("hardware decoder unavailable");
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
        au.times.recv_us = recv;
        if self.decoder.is_none() {
            self.video_without_decoder += 1;
            if self.video_without_decoder == 1 || self.video_without_decoder % 120 == 0 {
                warn!(
                    n = self.video_without_decoder,
                    w = au.width,
                    h = au.height,
                    key = au.is_keyframe,
                    "video arrived before decoder; requesting IDR after CapsAccept"
                );
            }
            if au.is_keyframe && au.width > 0 {
                // Last-resort: still no CapsAccept. Decode 4:2:0 at the AU size.
                self.open_decoder(lansec_protocol::Chroma::Yuv420, au.width as u32, au.height as u32);
                if let Ok(bytes) = encode(&ControlMsg::RequestIdr) {
                    let _ = self.bud.send(Channel::Control, &bytes, 0, true);
                }
            }
        }
        if let Some(dec) = self.decoder.as_mut() {
            match dec.decode(&au.annexb, au.is_keyframe) {
                Ok(Some(frame)) => {
                    au.times.decode_done_us = frame.decode_done_us.max(recv);
                    self.presenter.submit(frame, au.times);
                    if let Some(frame) = self.presenter.take() {
                        self.present_frame(frame);
                    }
                }
                Ok(None) => {}
                Err(e) => warn!("decode: {e}"),
            }
        }
        self.video_frames += 1;
        if self.last_cong.elapsed().as_millis() >= 100 {
            self.last_cong = Instant::now();
            if let Ok(report) = encode(&ControlMsg::Congestion(self.bud.congestion.lock().report())) {
                let _ = self.bud.send(Channel::Control, &report, 0, true);
            }
            let t = self.presenter.last_times();
            if self.video_frames % 120 == 1 {
                eprintln!(
                    "timing capture→encode {:.1}ms net {:.1}ms decode {:.1}ms glass {:.1}ms dropped {}",
                    t.capture_to_encode_ms(),
                    t.net_ms(),
                    t.decode_ms(),
                    t.glass_ms(),
                    self.presenter.stats().dropped
                );
            }
        }
    }

    fn present_frame(&mut self, frame: lansec_decode::DecodedFrame) {
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
        let _ = frame;
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
        self.pump();
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
                self.send_input(&InputEvent::MouseMoveAbs {
                    x: position.x as u16,
                    y: position.y as u16,
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
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }
}

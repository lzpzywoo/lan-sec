use std::time::Duration;

use anyhow::{Context, Result};
use lansec_protocol::{Caps, Chroma, Platform};
use tracing::info;

mod client;
mod host;
mod keys;

pub use client::run_client;
pub use host::run_host;

pub fn local_caps() -> Caps {
    Caps {
        encode: lansec_encode::probe_encode(),
        decode: lansec_decode::probe_decode(),
        audio: true,
        input: true,
        platform: Platform::current(),
    }
}

pub fn print_probe() {
    let caps = local_caps();
    println!("platform: {:?}", caps.platform);
    println!("audio: {}  input: {}", caps.audio, caps.input);
    println!("encode:");
    for c in &caps.encode {
        println!(
            "  {:?} {:?} {}-bit {}x{} {:?}",
            c.codec, c.chroma, c.bit_depth, c.max_width, c.max_height, c.encode
        );
    }
    println!("decode:");
    for c in &caps.decode {
        println!(
            "  {:?} {:?} {}-bit {}x{} {:?}",
            c.codec, c.chroma, c.bit_depth, c.max_width, c.max_height, c.decode
        );
    }
    if caps.encode.iter().any(|c| c.chroma == Chroma::Yuv444) {
        println!("true-color 4:4:4 encode: advertised (hardware)");
    } else {
        println!("true-color 4:4:4 encode: not available (will negotiate 4:2:0)");
    }
    if caps.decode.iter().any(|c| c.chroma == Chroma::Yuv444) {
        println!("true-color 4:4:4 decode: advertised");
    } else {
        println!("true-color 4:4:4 decode: 4:2:0 only");
    }
}

pub fn run_loopback() -> Result<()> {
    info!("local capture → encode → decode → present (no network)");
    let mut capture = lansec_capture::CaptureSession::open().context("capture")?;
    let (w, h) = capture.size();
    let cfg = lansec_encode::EncoderConfig {
        width: w,
        height: h,
        bitrate_bps: 40_000_000,
        prefer_444: true,
    };
    #[cfg(windows)]
    let mut encoder = lansec_encode::open_encoder_with_gpu(capture.gpu(), cfg)?;
    #[cfg(not(windows))]
    let mut encoder = lansec_encode::open_encoder(cfg)?;
    info!(backend = ?encoder.backend(), chroma = ?encoder.chroma(), "loopback encoder");
    println!("loopback chroma: {:?}", encoder.chroma());

#[cfg(windows)]
    let mut decoder = lansec_decode::open_decoder_with_gpu(capture.gpu(), encoder.chroma(), w, h).ok();
    #[cfg(target_os = "macos")]
    let mut decoder = lansec_decode::open_decoder(encoder.chroma(), w, h).ok();
    #[cfg(not(any(windows, target_os = "macos")))]
    let mut decoder: Option<Box<dyn lansec_decode::HardwareDecoder>> = None;
    let mut presenter = lansec_present::Presenter::new(60.0);

    #[cfg(windows)]
    let swap = {
        let hwnd = lansec_present::windows_swapchain::Swapchain::create_window("lansec loopback", w, h).ok();
        match (hwnd, decoder.as_ref()) {
            (Some(hwnd), Some(_)) => lansec_present::windows_swapchain::Swapchain::from_hwnd(
                &capture.gpu().device,
                capture.gpu().context.clone(),
                hwnd,
                w,
                h,
            )
            .ok(),
            _ => None,
        }
    };

    let mut n = 0u32;
    let mut decoded = 0u32;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if let Some(frame) = capture.next_frame()? {
            if let Some(au) = encoder.encode(&frame, n == 0)? {
                n += 1;
                if n % 30 == 1 {
                    info!(n, bytes = au.annexb.len(), key = au.is_keyframe, "encoded");
                }
                if let Some(dec) = decoder.as_mut() {
                    if let Ok(Some(df)) = dec.decode(&au.annexb, au.is_keyframe) {
                        decoded += 1;
                        presenter.submit(df, lansec_protocol::FrameTimes::default());
                        if let Some(ready) = presenter.take() {
                            #[cfg(windows)]
                            if let (Some(swap), Some(tex)) = (swap.as_ref(), ready.d3d11_texture()) {
                                let _ = swap.blit_and_present(tex);
                            }
                            let _ = ready;
                        }
                    }
                }
            }
        }
    }
    info!(encoded = n, decoded, dropped = presenter.stats().dropped, "loopback done");
    println!("loopback encoded={n} decoded={decoded} chroma={:?}", encoder.chroma());
    Ok(())
}

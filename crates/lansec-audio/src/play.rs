use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::AudioError;

pub struct Player {
    _stream: cpal::Stream,
    queue: Arc<Mutex<VecDeque<f32>>>,
}

impl Player {
    pub fn start() -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| AudioError::Message("no output device".into()))?;
        let cfg = device
            .default_output_config()
            .map_err(|e| AudioError::Message(e.to_string()))?;
        let mut stream_cfg: cpal::StreamConfig = cfg.config();
        stream_cfg.channels = 2;
        stream_cfg.sample_rate = cpal::SampleRate(48_000);
        let queue = Arc::new(Mutex::new(VecDeque::with_capacity(48_000 * 2)));
        let q = queue.clone();
        let stream = device
            .build_output_stream(
                &stream_cfg,
                move |data: &mut [f32], _| {
                    let mut buf = q.lock().unwrap();
                    for s in data.iter_mut() {
                        *s = buf.pop_front().unwrap_or(0.0);
                    }
                },
                |e| tracing::warn!("audio out: {e}"),
                None,
            )
            .map_err(|e| AudioError::Message(e.to_string()))?;
        stream.play().map_err(|e| AudioError::Message(e.to_string()))?;
        tracing::info!("cpal playback 48 kHz stereo");
        Ok(Self {
            _stream: stream,
            queue,
        })
    }

    pub fn push(&self, pcm: &[f32]) {
        let mut buf = self.queue.lock().unwrap();
        if buf.len() > 48_000 * 4 {
            buf.clear();
        }
        buf.extend(pcm.iter().copied());
    }
}

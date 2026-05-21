use std::{
    collections::VecDeque,
    sync::{atomic::Ordering, Arc, Mutex},
    time::{Duration, Instant},
};

use screencapturekit::{
    cm::{CMSampleBuffer, CMSampleBufferExt},
    shareable_content::SCShareableContent,
    stream::{
        configuration::SCStreamConfiguration,
        content_filter::SCContentFilter,
        output_type::SCStreamOutputType,
        SCStream,
    },
};
use tauri::{AppHandle, Emitter};

use crate::{
    effective_elapsed_ms, levels_f32, MeterEvent, SharedAecProcessor, SharedSystemAudio,
    AEC_ENABLED, AEC_FRAME_SIZE,
};

pub struct SystemAudioCapture {
    stream: SCStream,
}

impl SystemAudioCapture {
    pub fn start(
        sample_rate: u32,
        system_audio: SharedSystemAudio,
        app: AppHandle,
        started_at: Instant,
        aec_processor: SharedAecProcessor,
    ) -> Result<Self, String> {
        let content = SCShareableContent::get()
            .map_err(|err| format!("ScreenCaptureKit content unavailable: {err}"))?;
        let display = content
            .displays()
            .into_iter()
            .next()
            .ok_or_else(|| "No displays available for ScreenCaptureKit".to_string())?;

        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        let config = SCStreamConfiguration::new()
            .with_width(2)
            .with_height(2)
            .with_captures_audio(true)
            .with_sample_rate(sample_rate as i32)
            .with_channel_count(2);

        let mut stream = SCStream::new(&filter, &config);

        let max_buffered = sample_rate as usize * 3;
        let last_emit = Arc::new(Mutex::new(Instant::now()));
        let render_buf: Arc<Mutex<VecDeque<f32>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(AEC_FRAME_SIZE * 4)));

        let handler_state = system_audio.clone();
        let handler_app = app.clone();
        let handler_last_emit = last_emit.clone();
        let handler_processor = aec_processor.clone();
        let handler_render_buf = render_buf.clone();
        let handler =
            move |sample_buffer: CMSampleBuffer, of_type: SCStreamOutputType| {
                if of_type != SCStreamOutputType::Audio {
                    return;
                }

                let Some(buffer_list) = sample_buffer.audio_buffer_list() else {
                    return;
                };

                let mut mono: Vec<f32> = Vec::new();
                let buffers: Vec<&_> = buffer_list.iter().collect();

                if buffers.is_empty() {
                    return;
                }

                if buffers.len() == 1 {
                    let buf = buffers[0];
                    let samples = bytes_to_f32(buf.data());
                    let channels = buf.number_channels.max(1) as usize;
                    mono.reserve(samples.len() / channels);
                    for frame in samples.chunks(channels) {
                        let mixed = frame.iter().copied().sum::<f32>() / frame.len() as f32;
                        mono.push(mixed.clamp(-1.0, 1.0));
                    }
                } else {
                    let channel_samples: Vec<&[f32]> =
                        buffers.iter().map(|b| bytes_to_f32(b.data())).collect();
                    let frames = channel_samples.iter().map(|s| s.len()).min().unwrap_or(0);
                    mono.reserve(frames);
                    for i in 0..frames {
                        let sum: f32 = channel_samples.iter().map(|s| s[i]).sum();
                        let mixed = sum / channel_samples.len() as f32;
                        mono.push(mixed.clamp(-1.0, 1.0));
                    }
                }

                if mono.is_empty() {
                    return;
                }

                let (peak, rms) = levels_f32(&mono);

                // Feed AEC render (far-end) reference: 10ms blocks of system audio
                // so the WebRTC AudioProcessing module can learn the echo path
                // and cancel speaker→mic bleed when the mic side runs.
                if AEC_ENABLED.load(Ordering::Relaxed) {
                    if let Ok(mut rbuf) = handler_render_buf.lock() {
                        rbuf.extend(mono.iter().copied());
                        while rbuf.len() >= AEC_FRAME_SIZE {
                            let block: Vec<f32> = rbuf.drain(..AEC_FRAME_SIZE).collect();
                            let mut frame: Vec<Vec<f32>> = vec![block];
                            let _ = handler_processor.process_render_frame(&mut frame);
                        }
                    }
                }

                if let Ok(mut state) = handler_state.lock() {
                    state.peak = peak;
                    state.rms = rms;
                    state.samples.extend(mono);
                    while state.samples.len() > max_buffered {
                        state.samples.pop_front();
                    }
                }

                if let Ok(mut last) = handler_last_emit.lock() {
                    if last.elapsed() >= Duration::from_millis(250) {
                        let _ = handler_app.emit(
                            "audio-meter",
                            MeterEvent {
                                mic_peak: 0.0,
                                mic_rms: 0.0,
                                system_peak: peak,
                                system_rms: rms,
                                elapsed_ms: effective_elapsed_ms(started_at),
                            },
                        );
                        *last = Instant::now();
                    }
                }
            };

        stream.add_output_handler(handler, SCStreamOutputType::Audio);

        stream
            .start_capture()
            .map_err(|err| format!("Could not start ScreenCaptureKit stream: {err}"))?;

        Ok(Self { stream })
    }

    pub fn stop(self) -> Result<(), String> {
        self.stream
            .stop_capture()
            .map_err(|err| format!("Could not stop ScreenCaptureKit stream: {err}"))
    }
}

fn bytes_to_f32(bytes: &[u8]) -> &[f32] {
    let len = bytes.len() / std::mem::size_of::<f32>();
    if len == 0 {
        return &[];
    }
    let ptr = bytes.as_ptr();
    if (ptr as usize) % std::mem::align_of::<f32>() != 0 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(ptr.cast::<f32>(), len) }
}

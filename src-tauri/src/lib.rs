use std::{
    collections::VecDeque,
    fs::{self, File},
    io::BufWriter,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use chrono::Local;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    FromSample, Sample, SampleFormat, SizedSample, Stream,
};
#[cfg(target_os = "macos")]
use objc2::runtime::Bool;
#[cfg(target_os = "macos")]
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use once_cell::sync::Lazy;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

#[cfg(target_os = "macos")]
mod system_audio;
#[cfg(target_os = "macos")]
use system_audio::SystemAudioCapture;

const TARGET_SAMPLE_RATE: u32 = 48_000;

type SharedWriter = Arc<Mutex<Option<hound::WavWriter<BufWriter<File>>>>>;
type SharedSystemAudio = Arc<Mutex<SystemAudioState>>;

static RECORDER: Lazy<Mutex<Option<Recorder>>> = Lazy::new(|| Mutex::new(None));
static MIC_ENABLED: AtomicBool = AtomicBool::new(true);
static SYS_ENABLED: AtomicBool = AtomicBool::new(true);
static PAUSED: AtomicBool = AtomicBool::new(false);
static PAUSED_OFFSET_MS: AtomicU64 = AtomicU64::new(0);
static PAUSED_BEGAN_AT_MS: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "macos")]
pub(crate) static AEC_ENABLED: AtomicBool = AtomicBool::new(true);

#[cfg(target_os = "macos")]
pub(crate) const AEC_FRAME_SIZE: usize = 480; // 10 ms @ 48 kHz
#[cfg(target_os = "macos")]
pub(crate) type SharedAecProcessor = Arc<webrtc_audio_processing::Processor>;
#[cfg(target_os = "macos")]
pub(crate) type SharedMicBuf = Arc<Mutex<VecDeque<f32>>>;

pub(crate) fn effective_elapsed_ms(started_at: Instant) -> u128 {
    let raw = started_at.elapsed().as_millis();
    let off = PAUSED_OFFSET_MS.load(Ordering::Relaxed) as u128;
    let extra = if PAUSED.load(Ordering::Relaxed) {
        let now = started_at.elapsed().as_millis() as u64;
        let began = PAUSED_BEGAN_AT_MS.load(Ordering::Relaxed);
        (now.saturating_sub(began)) as u128
    } else {
        0
    };
    raw.saturating_sub(off).saturating_sub(extra)
}

#[derive(Default)]
struct SystemAudioState {
    samples: VecDeque<f32>,
    peak: f32,
    rms: f32,
}

#[derive(Serialize, Clone)]
struct MeterEvent {
    mic_peak: f32,
    mic_rms: f32,
    system_peak: f32,
    system_rms: f32,
    elapsed_ms: u128,
}

#[derive(Serialize)]
struct RecordingStarted {
    path: String,
    sample_rate: u32,
    channels: u16,
    input_device: String,
    system_audio: String,
}

#[derive(Serialize)]
struct RecordingStopped {
    path: String,
    duration_ms: u128,
}

struct Recorder {
    path: PathBuf,
    started_at: Instant,
    streams: Vec<Stream>,
    writer: SharedWriter,
    #[cfg(target_os = "macos")]
    system_capture: Option<SystemAudioCapture>,
    #[cfg(target_os = "macos")]
    _aec: SharedAecProcessor,
}

// CPAL's CoreAudio stream contains non-Send callback plumbing, but this app only
// stores it to keep the stream alive and drops it while guarded by RECORDER.
unsafe impl Send for Recorder {}

#[tauri::command]
fn start_recording(app: AppHandle) -> Result<RecordingStarted, String> {
    ensure_microphone_permission()?;
    #[cfg(target_os = "macos")]
    ensure_screen_capture_permission()?;

    let mut recorder = RECORDER
        .lock()
        .map_err(|_| "Recorder state is locked".to_string())?;

    if recorder.is_some() {
        return Err("Recording is already running".to_string());
    }

    PAUSED.store(false, Ordering::Relaxed);
    PAUSED_OFFSET_MS.store(0, Ordering::Relaxed);
    PAUSED_BEGAN_AT_MS.store(0, Ordering::Relaxed);

    #[cfg(target_os = "macos")]
    let aec_processor: SharedAecProcessor = {
        use webrtc_audio_processing::{config::EchoCanceller, Config, Processor};
        let processor = Processor::new(TARGET_SAMPLE_RATE)
            .map_err(|err| format!("Could not initialize AEC processor: {err}"))?;
        let config = Config {
            echo_canceller: Some(EchoCanceller::default()),
            ..Default::default()
        };
        processor.set_config(config);
        Arc::new(processor)
    };
    #[cfg(target_os = "macos")]
    let mic_buf: SharedMicBuf = Arc::new(Mutex::new(VecDeque::with_capacity(
        AEC_FRAME_SIZE * 4,
    )));

    let host = cpal::default_host();
    let mic_device = host
        .default_input_device()
        .ok_or_else(|| "No default input device found".to_string())?;
    let input_device = mic_device
        .name()
        .unwrap_or_else(|_| "Unknown input device".to_string());
    let mic_default_config = mic_device
        .default_input_config()
        .map_err(|err| format!("Could not read default input config: {err}"))?;

    let mic_stream_config = cpal::StreamConfig {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        buffer_size: cpal::BufferSize::Default,
    };

    let path = recording_path()?;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let writer = hound::WavWriter::create(&path, spec)
        .map_err(|err| format!("Could not create WAV file: {err}"))?;
    let writer: SharedWriter = Arc::new(Mutex::new(Some(writer)));
    let system_audio: SharedSystemAudio = Arc::new(Mutex::new(SystemAudioState {
        samples: VecDeque::with_capacity(TARGET_SAMPLE_RATE as usize * 2),
        peak: 0.0,
        rms: 0.0,
    }));
    let started_at = Instant::now();

    let mic_stream = match mic_default_config.sample_format() {
        SampleFormat::F32 => build_mic_stream::<f32>(
            &mic_device,
            &mic_stream_config,
            writer.clone(),
            system_audio.clone(),
            app.clone(),
            started_at,
            #[cfg(target_os = "macos")]
            aec_processor.clone(),
            #[cfg(target_os = "macos")]
            mic_buf.clone(),
        ),
        SampleFormat::I16 => build_mic_stream::<i16>(
            &mic_device,
            &mic_stream_config,
            writer.clone(),
            system_audio.clone(),
            app.clone(),
            started_at,
            #[cfg(target_os = "macos")]
            aec_processor.clone(),
            #[cfg(target_os = "macos")]
            mic_buf.clone(),
        ),
        SampleFormat::U16 => build_mic_stream::<u16>(
            &mic_device,
            &mic_stream_config,
            writer.clone(),
            system_audio.clone(),
            app.clone(),
            started_at,
            #[cfg(target_os = "macos")]
            aec_processor.clone(),
            #[cfg(target_os = "macos")]
            mic_buf.clone(),
        ),
        sample_format => Err(format!("Unsupported sample format: {sample_format:?}")),
    }?;

    let system_audio_status;
    #[cfg(target_os = "macos")]
    let system_capture = match SystemAudioCapture::start(
        TARGET_SAMPLE_RATE,
        system_audio.clone(),
        app.clone(),
        started_at,
        aec_processor.clone(),
    ) {
        Ok(capture) => {
            system_audio_status = "ScreenCaptureKit @ 48 kHz".to_string();
            Some(capture)
        }
        Err(err) => {
            system_audio_status = format!("System audio unavailable: {err}");
            None
        }
    };
    #[cfg(not(target_os = "macos"))]
    {
        system_audio_status = "System audio capture only available on macOS".to_string();
    }

    mic_stream
        .play()
        .map_err(|err| format!("Could not start input stream: {err}"))?;
    let streams = vec![mic_stream];

    *recorder = Some(Recorder {
        path: path.clone(),
        started_at,
        streams,
        writer,
        #[cfg(target_os = "macos")]
        system_capture,
        #[cfg(target_os = "macos")]
        _aec: aec_processor,
    });

    Ok(RecordingStarted {
        path: path.to_string_lossy().to_string(),
        sample_rate: TARGET_SAMPLE_RATE,
        channels: 1,
        input_device,
        system_audio: system_audio_status,
    })
}

#[tauri::command]
fn stop_recording() -> Result<RecordingStopped, String> {
    let recorder = RECORDER
        .lock()
        .map_err(|_| "Recorder state is locked".to_string())?
        .take();
    let recorder = recorder.ok_or_else(|| "No recording is running".to_string())?;

    let duration_ms = effective_elapsed_ms(recorder.started_at);
    PAUSED.store(false, Ordering::Relaxed);
    PAUSED_OFFSET_MS.store(0, Ordering::Relaxed);
    PAUSED_BEGAN_AT_MS.store(0, Ordering::Relaxed);

    #[cfg(target_os = "macos")]
    if let Some(capture) = recorder.system_capture {
        let _ = capture.stop();
    }
    drop(recorder.streams);

    let mut writer = recorder
        .writer
        .lock()
        .map_err(|_| "Recorder writer is locked".to_string())?;
    if let Some(writer) = writer.take() {
        writer
            .finalize()
            .map_err(|err| format!("Could not finalize WAV file: {err}"))?;
    }

    Ok(RecordingStopped {
        path: recorder.path.to_string_lossy().to_string(),
        duration_ms,
    })
}

#[tauri::command]
fn pause_recording() -> Result<(), String> {
    let recorder_guard = RECORDER
        .lock()
        .map_err(|_| "Recorder state is locked".to_string())?;
    let recorder = recorder_guard
        .as_ref()
        .ok_or_else(|| "No recording is running".to_string())?;
    if PAUSED.load(Ordering::Relaxed) {
        return Ok(());
    }
    let now_ms = recorder.started_at.elapsed().as_millis() as u64;
    PAUSED_BEGAN_AT_MS.store(now_ms, Ordering::Relaxed);
    PAUSED.store(true, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
fn resume_recording() -> Result<(), String> {
    let recorder_guard = RECORDER
        .lock()
        .map_err(|_| "Recorder state is locked".to_string())?;
    let recorder = recorder_guard
        .as_ref()
        .ok_or_else(|| "No recording is running".to_string())?;
    if !PAUSED.load(Ordering::Relaxed) {
        return Ok(());
    }
    let now_ms = recorder.started_at.elapsed().as_millis() as u64;
    let began = PAUSED_BEGAN_AT_MS.load(Ordering::Relaxed);
    let delta = now_ms.saturating_sub(began);
    PAUSED_OFFSET_MS.fetch_add(delta, Ordering::Relaxed);
    PAUSED.store(false, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
fn set_mix_gates(mic: bool, system: bool) {
    MIC_ENABLED.store(mic, Ordering::Relaxed);
    SYS_ENABLED.store(system, Ordering::Relaxed);
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn set_aec_enabled(enabled: bool) {
    AEC_ENABLED.store(enabled, Ordering::Relaxed);
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
fn set_aec_enabled(_enabled: bool) {}

#[cfg(target_os = "macos")]
fn run_tccutil_reset() {
    // Resets TCC entries for our bundle id so that stale "phantom" toggles
    // (from previous rebuilds with different code signatures) don't block us.
    // tccutil reset does NOT require privileges when resetting your own bundle id.
    let services = ["ScreenCapture", "Microphone"];
    for svc in services {
        let _ = Command::new("tccutil")
            .args(["reset", svc, APP_BUNDLE_ID])
            .output();
    }
}

const APP_BUNDLE_ID: &str = "io.dbr.rekk";

#[cfg(target_os = "macos")]
#[tauri::command]
fn reset_permissions() -> Result<(), String> {
    run_tccutil_reset();
    Ok(())
}

#[tauri::command]
fn restart_app(app: AppHandle) {
    // macOS caches TCC permission state for the lifetime of the process, so
    // after tccutil reset the running app still reports the old state.
    // Restart is the only way to read a fresh state from the kernel.
    app.restart();
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn request_mic_permission() -> Result<String, String> {
    // Triggers the AVCaptureDevice permission flow. If macOS has already
    // recorded a decision (Authorized / Denied) this just returns it; only on
    // NotDetermined does the OS show the system prompt.
    let outcome = match ensure_microphone_permission() {
        Ok(()) => "authorized".to_string(),
        Err(err) => format!("denied: {err}"),
    };
    Ok(outcome)
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn request_screen_permission() -> Result<String, String> {
    // CGRequestScreenCaptureAccess shows the system prompt only the first time.
    // If the user previously denied, returns false silently; in that case the
    // user must toggle the entry under System Settings (use open_privacy_settings).
    let outcome = match ensure_screen_capture_permission() {
        Ok(()) => "authorized".to_string(),
        Err(err) => format!("denied: {err}"),
    };
    Ok(outcome)
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn open_privacy_settings(panel: String) -> Result<(), String> {
    let url = match panel.as_str() {
        "screen" => "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
        "microphone" => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        }
        other => return Err(format!("Unknown privacy panel: {other}")),
    };
    Command::new("open")
        .arg(url)
        .spawn()
        .map_err(|err| format!("Could not open System Settings: {err}"))?;
    Ok(())
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn setup_status() -> Result<SetupStatus, String> {
    Ok(SetupStatus {
        mic_permission: mic_permission_string(),
        screen_permission: screen_permission_string(),
    })
}

#[cfg(target_os = "macos")]
#[derive(Serialize)]
struct SetupStatus {
    mic_permission: String,
    screen_permission: String,
}

#[cfg(target_os = "macos")]
fn mic_permission_string() -> String {
    let media_type = match unsafe { AVMediaTypeAudio } {
        Some(t) => t,
        None => return "unknown".to_string(),
    };
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) } {
        AVAuthorizationStatus::Authorized => "authorized".to_string(),
        AVAuthorizationStatus::Denied => "denied".to_string(),
        AVAuthorizationStatus::Restricted => "restricted".to_string(),
        AVAuthorizationStatus::NotDetermined => "not_determined".to_string(),
        _ => "unknown".to_string(),
    }
}

#[cfg(target_os = "macos")]
fn screen_permission_string() -> String {
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
    }
    if unsafe { CGPreflightScreenCaptureAccess() } {
        "authorized".to_string()
    } else {
        "denied".to_string()
    }
}

fn build_mic_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    writer: SharedWriter,
    system_audio: SharedSystemAudio,
    app: AppHandle,
    started_at: Instant,
    #[cfg(target_os = "macos")] aec_processor: SharedAecProcessor,
    #[cfg(target_os = "macos")] mic_buf: SharedMicBuf,
) -> Result<Stream, String>
where
    T: Sample + SizedSample,
    i16: FromSample<T>,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let mut last_emit = Instant::now();

    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                let (mic_peak, mic_rms, system_peak, system_rms) = write_mixed_samples(
                    data,
                    channels,
                    &writer,
                    &system_audio,
                    #[cfg(target_os = "macos")]
                    &aec_processor,
                    #[cfg(target_os = "macos")]
                    &mic_buf,
                );

                if last_emit.elapsed() >= Duration::from_millis(33) {
                    let _ = app.emit(
                        "audio-meter",
                        MeterEvent {
                            mic_peak,
                            mic_rms,
                            system_peak,
                            system_rms,
                            elapsed_ms: effective_elapsed_ms(started_at),
                        },
                    );
                    last_emit = Instant::now();
                }
            },
            move |err| {
                eprintln!("Audio input stream error: {err}");
            },
            None,
        )
        .map_err(|err| format!("Could not build input stream: {err}"))
}

fn write_mixed_samples<T>(
    data: &[T],
    channels: usize,
    writer: &SharedWriter,
    system_audio: &SharedSystemAudio,
    #[cfg(target_os = "macos")] aec_processor: &SharedAecProcessor,
    #[cfg(target_os = "macos")] mic_buf: &SharedMicBuf,
) -> (f32, f32, f32, f32)
where
    T: Sample,
    f32: FromSample<T>,
{
    let mut raw_mic: Vec<f32> = Vec::with_capacity(data.len() / channels.max(1));
    for frame in data.chunks(channels.max(1)) {
        let m = frame
            .iter()
            .map(|sample| sample.to_sample::<f32>())
            .sum::<f32>()
            / frame.len().max(1) as f32;
        raw_mic.push(m);
    }

    let latest_system_peak;
    let latest_system_rms;
    {
        let state = system_audio.lock().ok();
        latest_system_peak = state.as_ref().map(|s| s.peak).unwrap_or(0.0);
        latest_system_rms = state.as_ref().map(|s| s.rms).unwrap_or(0.0);
    }

    let mic_gain = if MIC_ENABLED.load(Ordering::Relaxed) { 0.72 } else { 0.0 };
    let sys_gain = if SYS_ENABLED.load(Ordering::Relaxed) { 0.72 } else { 0.0 };
    let paused = PAUSED.load(Ordering::Relaxed);

    #[cfg(target_os = "macos")]
    let aec_on = AEC_ENABLED.load(Ordering::Relaxed);

    // Accumulate mic samples and process in 10ms blocks for AEC.
    #[cfg(target_os = "macos")]
    {
        let mut buf = match mic_buf.lock() {
            Ok(b) => b,
            Err(_) => return (0.0, 0.0, latest_system_peak, latest_system_rms),
        };
        buf.extend(raw_mic.iter().copied());

        let mut processed_mic: Vec<f32> = Vec::new();
        let mut consumed_sys: Vec<f32> = Vec::new();

        while buf.len() >= AEC_FRAME_SIZE {
            let mut mic_block: Vec<f32> = buf.drain(..AEC_FRAME_SIZE).collect();

            // Pull a matching block of system samples from the ring buffer.
            let mut sys_block: Vec<f32> = Vec::with_capacity(AEC_FRAME_SIZE);
            if let Ok(mut state) = system_audio.lock() {
                for _ in 0..AEC_FRAME_SIZE {
                    sys_block.push(state.samples.pop_front().unwrap_or(0.0));
                }
            } else {
                sys_block.resize(AEC_FRAME_SIZE, 0.0);
            }

            if aec_on && !paused {
                let mut frame: Vec<Vec<f32>> = vec![std::mem::take(&mut mic_block)];
                if aec_processor.process_capture_frame(&mut frame).is_ok() {
                    mic_block = frame.into_iter().next().unwrap_or_default();
                    if mic_block.len() != AEC_FRAME_SIZE {
                        mic_block.resize(AEC_FRAME_SIZE, 0.0);
                    }
                } else {
                    mic_block = frame.into_iter().next().unwrap_or_default();
                    if mic_block.len() != AEC_FRAME_SIZE {
                        mic_block.resize(AEC_FRAME_SIZE, 0.0);
                    }
                }
            }

            if !paused {
                if let Ok(mut writer_guard) = writer.lock() {
                    if let Some(writer) = writer_guard.as_mut() {
                        for i in 0..AEC_FRAME_SIZE {
                            let mixed =
                                (mic_block[i] * mic_gain + sys_block[i] * sys_gain).clamp(-1.0, 1.0);
                            let sample = (mixed * i16::MAX as f32) as i16;
                            let _ = writer.write_sample(sample);
                        }
                    }
                }
            }

            processed_mic.extend_from_slice(&mic_block);
            consumed_sys.extend_from_slice(&sys_block);
        }

        let (mic_peak, mic_rms) = levels_f32(&processed_mic);
        let (sys_peak_block, sys_rms_block) = levels_f32(&consumed_sys);
        let sys_peak = latest_system_peak.max(sys_peak_block);
        let sys_rms = latest_system_rms.max(sys_rms_block);
        return (mic_peak, mic_rms, sys_peak, sys_rms);
    }

    #[cfg(not(target_os = "macos"))]
    {
        let mut system_state = system_audio.lock().ok();
        let mut writer_guard = writer.lock().ok();

        if !paused {
            if let Some(Some(writer)) = writer_guard.as_mut().map(|g| g.as_mut()) {
                for mic in &raw_mic {
                    let system = system_state
                        .as_mut()
                        .and_then(|state| state.samples.pop_front())
                        .unwrap_or(0.0);
                    let mixed = (mic * mic_gain + system * sys_gain).clamp(-1.0, 1.0);
                    let sample = (mixed * i16::MAX as f32) as i16;
                    let _ = writer.write_sample(sample);
                }
            }
        } else if let Some(state) = system_state.as_mut() {
            for _ in 0..raw_mic.len() {
                state.samples.pop_front();
            }
        }

        let (mic_peak, mic_rms) = levels_f32(&raw_mic);
        (mic_peak, mic_rms, latest_system_peak, latest_system_rms)
    }
}

fn levels_f32(data: &[f32]) -> (f32, f32) {
    if data.is_empty() {
        return (0.0, 0.0);
    }

    let mut peak = 0.0_f32;
    let mut sum = 0.0_f32;

    for sample in data {
        let level = sample.abs().min(1.0);
        peak = peak.max(level);
        sum += level * level;
    }

    (peak, (sum / data.len() as f32).sqrt().min(1.0))
}

#[allow(dead_code)]
fn levels<T>(data: &[T], channels: usize) -> (f32, f32)
where
    T: Sample,
    f32: FromSample<T>,
{
    if data.is_empty() {
        return (0.0, 0.0);
    }

    let mut mixed = Vec::with_capacity(data.len() / channels.max(1));
    for frame in data.chunks(channels.max(1)) {
        mixed.push(
            frame
                .iter()
                .map(|sample| sample.to_sample::<f32>())
                .sum::<f32>()
                / frame.len().max(1) as f32,
        );
    }
    levels_f32(&mixed)
}

fn recording_dir() -> Result<PathBuf, String> {
    let base = dirs::audio_dir()
        .or_else(dirs::document_dir)
        .ok_or_else(|| "Could not find an audio/documents directory".to_string())?;
    let dir = base.join("Rek");
    fs::create_dir_all(&dir).map_err(|err| format!("Could not create recordings directory: {err}"))?;
    Ok(dir)
}

fn recording_path() -> Result<PathBuf, String> {
    let dir = recording_dir()?;
    let filename = format!("rek-{}.wav", Local::now().format("%Y%m%d-%H%M%S"));
    Ok(dir.join(filename))
}

#[derive(Serialize)]
struct RecordingEntry {
    path: String,
    name: String,
    duration_ms: u64,
    size_bytes: u64,
    modified_secs: i64,
    has_transcript: bool,
}

#[tauri::command]
fn list_recordings() -> Result<Vec<RecordingEntry>, String> {
    let dir = recording_dir()?;
    let mut out: Vec<RecordingEntry> = Vec::new();
    let entries = fs::read_dir(&dir).map_err(|err| format!("Could not read recordings dir: {err}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("wav") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let modified_secs = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let duration_ms = match hound::WavReader::open(&path) {
            Ok(reader) => {
                let spec = reader.spec();
                let frames = reader.len() as u64 / spec.channels.max(1) as u64;
                if spec.sample_rate > 0 {
                    (frames * 1000) / spec.sample_rate as u64
                } else {
                    0
                }
            }
            Err(_) => 0,
        };
        let txt_path = path.with_extension("txt");
        out.push(RecordingEntry {
            path: path.to_string_lossy().to_string(),
            name,
            duration_ms,
            size_bytes: meta.len(),
            modified_secs,
            has_transcript: txt_path.exists(),
        });
    }
    out.sort_by(|a, b| b.modified_secs.cmp(&a.modified_secs));
    Ok(out)
}

#[tauri::command]
fn read_transcript(path: String) -> Result<String, String> {
    let audio_path = PathBuf::from(path);
    let txt_path = audio_path.with_extension("txt");
    if !txt_path.exists() {
        return Ok(String::new());
    }
    fs::read_to_string(&txt_path).map_err(|err| format!("Could not read transcript: {err}"))
}

#[tauri::command]
fn open_recordings_dir() -> Result<String, String> {
    let dir = recording_dir()?;
    let dir_str = dir.to_string_lossy().to_string();
    Command::new("open")
        .arg(&dir)
        .spawn()
        .map_err(|err| format!("Could not open Finder: {err}"))?;
    Ok(dir_str)
}

#[tauri::command]
fn reveal_recording(path: String) -> Result<(), String> {
    let audio_path = PathBuf::from(&path);
    let dir = recording_dir()?;
    if !audio_path.starts_with(&dir) {
        return Err("Refusing to reveal file outside Rek directory".to_string());
    }
    if !audio_path.exists() {
        return Err("File does not exist".to_string());
    }
    Command::new("open")
        .arg("-R")
        .arg(&audio_path)
        .spawn()
        .map_err(|err| format!("Could not reveal in Finder: {err}"))?;
    Ok(())
}

#[tauri::command]
fn delete_recording(path: String) -> Result<(), String> {
    let audio_path = PathBuf::from(&path);
    let dir = recording_dir()?;
    if !audio_path.starts_with(&dir) {
        return Err("Refusing to delete file outside Rek directory".to_string());
    }
    if audio_path.exists() {
        fs::remove_file(&audio_path).map_err(|err| format!("Could not delete recording: {err}"))?;
    }
    let txt_path = audio_path.with_extension("txt");
    if txt_path.exists() {
        let _ = fs::remove_file(&txt_path);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn ensure_microphone_permission() -> Result<(), String> {
    use std::sync::mpsc;

    let media_type = unsafe { AVMediaTypeAudio.ok_or("AVMediaTypeAudio is unavailable")? };
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };

    match status {
        AVAuthorizationStatus::Authorized => Ok(()),
        AVAuthorizationStatus::Denied => Err(
            "Microphone permission is denied for Rek. Enable it in macOS Settings > Privacy & Security > Microphone, then restart the app."
                .to_string(),
        ),
        AVAuthorizationStatus::Restricted => {
            Err("Microphone permission is restricted by macOS.".to_string())
        }
        AVAuthorizationStatus::NotDetermined => {
            let (tx, rx) = mpsc::channel();
            let block = block2::RcBlock::<dyn Fn(Bool)>::new(move |granted: Bool| {
                let _ = tx.send(granted.as_bool());
            });

            unsafe {
                AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &block);
            }

            let granted = rx
                .recv_timeout(Duration::from_secs(60))
                .map_err(|_| "Timed out waiting for microphone permission.".to_string())?;

            if granted {
                Ok(())
            } else {
                Err("Microphone permission was not granted.".to_string())
            }
        }
        other => Err(format!("Unknown microphone permission state: {other:?}")),
    }
}

#[cfg(not(target_os = "macos"))]
fn ensure_microphone_permission() -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn ensure_screen_capture_permission() -> Result<(), String> {
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    let granted = unsafe { CGPreflightScreenCaptureAccess() };
    if granted {
        return Ok(());
    }

    let requested = unsafe { CGRequestScreenCaptureAccess() };
    if requested {
        Ok(())
    } else {
        Err(
            "Screen Recording permission is required to capture system audio. Enable Rek under System Settings > Privacy & Security > Screen & System Audio Recording, then restart the app."
                .to_string(),
        )
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            start_recording,
            stop_recording,
            pause_recording,
            resume_recording,
            set_mix_gates,
            set_aec_enabled,
            list_recordings,
            read_transcript,
            delete_recording,
            open_recordings_dir,
            reveal_recording,
            setup_status,
            reset_permissions,
            open_privacy_settings,
            request_mic_permission,
            request_screen_permission,
            restart_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

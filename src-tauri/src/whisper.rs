use std::{
    fs::{self, File},
    io::{Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use once_cell::sync::Lazy;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

#[derive(Serialize, Clone, Copy)]
pub struct ModelPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub file: &'static str,
    pub bytes: u64,
    pub multilingual: bool,
}

pub const MODELS: &[ModelPreset] = &[
    ModelPreset {
        id: "tiny",
        label: "Tiny",
        file: "ggml-tiny.bin",
        bytes: 77_700_000,
        multilingual: true,
    },
    ModelPreset {
        id: "base",
        label: "Base",
        file: "ggml-base.bin",
        bytes: 147_900_000,
        multilingual: true,
    },
    ModelPreset {
        id: "small",
        label: "Small",
        file: "ggml-small.bin",
        bytes: 487_600_000,
        multilingual: true,
    },
    ModelPreset {
        id: "medium",
        label: "Medium",
        file: "ggml-medium.bin",
        bytes: 1_530_000_000,
        multilingual: true,
    },
    ModelPreset {
        id: "large-v3",
        label: "Large v3",
        file: "ggml-large-v3.bin",
        bytes: 3_100_000_000,
        multilingual: true,
    },
];

#[derive(Serialize, Clone)]
pub struct ModelStatus {
    pub id: String,
    pub label: String,
    pub file: String,
    pub bytes: u64,
    pub downloaded: bool,
    pub local_bytes: u64,
}

#[derive(Serialize, Clone)]
pub struct DownloadProgress {
    pub model: String,
    pub downloaded: u64,
    pub total: u64,
}

#[derive(Serialize, Clone)]
pub struct TranscriptSegment {
    pub path: String,
    pub index: usize,
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Serialize, Clone)]
pub struct TranscriptDone {
    pub path: String,
    pub language: String,
    pub segment_count: usize,
}

#[derive(Serialize, Clone)]
pub struct TranscriptResult {
    pub text_path: String,
    pub text: String,
    pub language: String,
    pub segment_count: usize,
}

#[derive(Serialize, Clone)]
pub struct TranscriptProgress {
    pub path: String,
    pub percent: i32,
}

#[derive(Serialize, Clone)]
pub struct TranscriptStarted {
    pub path: String,
    pub model: String,
    pub language: String,
    pub audio_seconds: f64,
}

struct CachedModel {
    id: String,
    ctx: WhisperContext,
}

static CACHED_MODEL: Lazy<Mutex<Option<CachedModel>>> = Lazy::new(|| Mutex::new(None));
static DOWNLOAD_CANCEL: AtomicBool = AtomicBool::new(false);
static DOWNLOAD_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static LAST_PROGRESS_EMIT: AtomicU64 = AtomicU64::new(0);

pub fn models_dir() -> Result<PathBuf, String> {
    let base = dirs::data_dir()
        .ok_or_else(|| "Could not resolve data directory".to_string())?;
    let dir = base.join("io.dbr.rekk").join("models");
    fs::create_dir_all(&dir).map_err(|err| format!("Could not create models dir: {err}"))?;
    Ok(dir)
}

pub fn model_path(model_id: &str) -> Result<PathBuf, String> {
    let preset = MODELS
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| format!("Unknown model: {model_id}"))?;
    Ok(models_dir()?.join(preset.file))
}

pub fn model_statuses() -> Result<Vec<ModelStatus>, String> {
    let dir = models_dir()?;
    Ok(MODELS
        .iter()
        .map(|preset| {
            let path = dir.join(preset.file);
            let (downloaded, local_bytes) = match fs::metadata(&path) {
                Ok(meta) => (meta.len() >= preset.bytes - preset.bytes / 5, meta.len()),
                Err(_) => (false, 0),
            };
            ModelStatus {
                id: preset.id.to_string(),
                label: preset.label.to_string(),
                file: preset.file.to_string(),
                bytes: preset.bytes,
                downloaded,
                local_bytes,
            }
        })
        .collect())
}

pub fn cancel_download() {
    DOWNLOAD_CANCEL.store(true, Ordering::Relaxed);
}

pub fn download_model(app: &AppHandle, model_id: &str) -> Result<(), String> {
    if DOWNLOAD_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return Err("A download is already in progress".to_string());
    }
    DOWNLOAD_CANCEL.store(false, Ordering::Relaxed);

    let result = download_model_inner(app, model_id);

    DOWNLOAD_IN_FLIGHT.store(false, Ordering::Relaxed);
    result
}

fn download_model_inner(app: &AppHandle, model_id: &str) -> Result<(), String> {
    let preset = MODELS
        .iter()
        .find(|m| m.id == model_id)
        .copied()
        .ok_or_else(|| format!("Unknown model: {model_id}"))?;
    let dir = models_dir()?;
    let final_path = dir.join(preset.file);
    let tmp_path = dir.join(format!("{}.part", preset.file));

    let url = format!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
        preset.file
    );

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(120))
        .build();
    let response = agent
        .get(&url)
        .call()
        .map_err(|err| format!("Download request failed: {err}"))?;
    let total: u64 = response
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(preset.bytes);

    let mut reader = response.into_reader();
    let mut file = File::create(&tmp_path)
        .map_err(|err| format!("Could not create temp file: {err}"))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut downloaded: u64 = 0;

    loop {
        if DOWNLOAD_CANCEL.load(Ordering::Relaxed) {
            drop(file);
            let _ = fs::remove_file(&tmp_path);
            return Err("Download cancelled".to_string());
        }
        let n = reader
            .read(&mut buf)
            .map_err(|err| format!("Download read error: {err}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|err| format!("Download write error: {err}"))?;
        downloaded += n as u64;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = LAST_PROGRESS_EMIT.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) >= 120 || downloaded == total {
            LAST_PROGRESS_EMIT.store(now_ms, Ordering::Relaxed);
            let _ = app.emit(
                "model-download-progress",
                DownloadProgress {
                    model: model_id.to_string(),
                    downloaded,
                    total,
                },
            );
        }
    }

    file.flush().ok();
    drop(file);
    fs::rename(&tmp_path, &final_path)
        .map_err(|err| format!("Could not finalize model: {err}"))?;

    Ok(())
}

pub fn load_model(model_id: &str) -> Result<(), String> {
    let path = model_path(model_id)?;
    if !path.exists() {
        return Err(format!("Model {model_id} is not downloaded yet"));
    }
    let mut guard = CACHED_MODEL
        .lock()
        .map_err(|_| "Model cache is locked".to_string())?;
    if let Some(cached) = guard.as_ref() {
        if cached.id == model_id {
            return Ok(());
        }
    }
    let ctx = WhisperContext::new_with_params(
        path.to_string_lossy().as_ref(),
        WhisperContextParameters::default(),
    )
    .map_err(|err| format!("Could not load model {model_id}: {err}"))?;
    *guard = Some(CachedModel {
        id: model_id.to_string(),
        ctx,
    });
    Ok(())
}

pub fn transcribe(
    app: &AppHandle,
    path: &PathBuf,
    model_id: &str,
    language: &str,
) -> Result<TranscriptResult, String> {
    load_model(model_id)?;
    let samples = read_wav_as_mono_16k_f32(path)?;
    let audio_seconds = samples.len() as f64 / 16_000.0;

    let path_str = path.to_string_lossy().to_string();

    // emit "started" so the UI can show a visible spinner immediately,
    // before we wait on the (potentially slow) model load + inference.
    let _ = app.emit(
        "transcript-started",
        &TranscriptStarted {
            path: path_str.clone(),
            model: model_id.to_string(),
            language: language.to_string(),
            audio_seconds,
        },
    );

    let guard = CACHED_MODEL
        .lock()
        .map_err(|_| "Model cache is locked".to_string())?;
    let cached = guard
        .as_ref()
        .ok_or_else(|| "Model not loaded".to_string())?;

    let mut state = cached
        .ctx
        .create_state()
        .map_err(|err| format!("Could not create whisper state: {err}"))?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    let lang_arg: Option<&str> = if language == "auto" || language.is_empty() {
        None
    } else {
        Some(language)
    };
    params.set_language(lang_arg);
    params.set_translate(false);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    // Shared state captured by the callbacks (must be 'static + Send + Sync).
    let segments_buf: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let segment_index = Arc::new(AtomicUsize::new(0));

    {
        let app_for_seg = app.clone();
        let path_for_seg = path_str.clone();
        let segments_for_cb = segments_buf.clone();
        let index_for_cb = segment_index.clone();
        params.set_segment_callback_safe(move |seg: whisper_rs::SegmentCallbackData| {
            let text = seg.text.trim().to_string();
            if text.is_empty() {
                return;
            }
            let idx = index_for_cb.fetch_add(1, Ordering::Relaxed);
            let event = TranscriptSegment {
                path: path_for_seg.clone(),
                index: idx,
                start: seg.start_timestamp as f64 / 100.0,
                end: seg.end_timestamp as f64 / 100.0,
                text: text.clone(),
            };
            let _ = app_for_seg.emit("transcript-segment", &event);
            if let Ok(mut b) = segments_for_cb.lock() {
                b.push(text);
            }
        });

        let app_for_prog = app.clone();
        let path_for_prog = path_str.clone();
        let last_emit = Arc::new(AtomicU64::new(0));
        let last_pct = Arc::new(AtomicU64::new(u64::MAX));
        params.set_progress_callback_safe(move |pct: i32| {
            // throttle: emit at most every 80 ms and only when % actually changes
            let pct_u = pct.max(0).min(100) as u64;
            if last_pct.load(Ordering::Relaxed) == pct_u {
                return;
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if now.saturating_sub(last_emit.load(Ordering::Relaxed)) < 80 && pct_u != 100 {
                return;
            }
            last_emit.store(now, Ordering::Relaxed);
            last_pct.store(pct_u, Ordering::Relaxed);
            let _ = app_for_prog.emit(
                "transcript-progress",
                TranscriptProgress {
                    path: path_for_prog.clone(),
                    percent: pct,
                },
            );
        });
    }

    state
        .full(params, &samples)
        .map_err(|err| format!("Whisper inference failed: {err}"))?;

    let detected_lang = whisper_rs::get_lang_str(state.full_lang_id_from_state())
        .unwrap_or("")
        .to_string();

    let full_lines = segments_buf.lock().map(|b| b.clone()).unwrap_or_default();
    let text = full_lines.join("\n");
    let text_path = path.with_extension("txt");
    fs::write(&text_path, &text).map_err(|err| format!("Could not write transcript: {err}"))?;

    let _ = app.emit(
        "transcript-done",
        &TranscriptDone {
            path: path_str.clone(),
            language: detected_lang.clone(),
            segment_count: full_lines.len(),
        },
    );

    Ok(TranscriptResult {
        text_path: text_path.to_string_lossy().to_string(),
        text,
        language: detected_lang,
        segment_count: full_lines.len(),
    })
}

/// Reads a 16-bit PCM mono WAV and returns f32 samples resampled to 16 kHz.
/// The recorder writes 48 kHz mono WAVs, so this does a 3:1 decimation with a
/// simple boxcar low-pass (sufficient for speech recognition; whisper.cpp
/// downstream has its own mel spectrogram + log-mel computation).
fn read_wav_as_mono_16k_f32(path: &PathBuf) -> Result<Vec<f32>, String> {
    let mut reader = hound::WavReader::open(path)
        .map_err(|err| format!("Could not open WAV: {err}"))?;
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;
    let sample_rate = spec.sample_rate;

    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => match spec.bits_per_sample {
            16 => reader
                .samples::<i16>()
                .map(|s| s.map(|v| v as f32 / i16::MAX as f32))
                .collect::<Result<_, _>>()
                .map_err(|err| format!("WAV read error: {err}"))?,
            24 | 32 => reader
                .samples::<i32>()
                .map(|s| s.map(|v| (v as f64 / i32::MAX as f64) as f32))
                .collect::<Result<_, _>>()
                .map_err(|err| format!("WAV read error: {err}"))?,
            other => {
                return Err(format!("Unsupported PCM bit depth: {other}"));
            }
        },
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .map_err(|err| format!("WAV read error: {err}"))?,
    };

    let mut mono: Vec<f32> = if channels <= 1 {
        raw
    } else {
        raw.chunks(channels)
            .map(|frame| frame.iter().copied().sum::<f32>() / frame.len() as f32)
            .collect()
    };

    if sample_rate == 16_000 {
        return Ok(mono);
    }

    let ratio = sample_rate as f64 / 16_000.0;
    if (ratio - 3.0).abs() < 0.001 && mono.len() >= 3 {
        // 48 kHz → 16 kHz: 3-sample boxcar then decimate
        let mut out = Vec::with_capacity(mono.len() / 3 + 1);
        let mut i = 0;
        while i + 2 < mono.len() {
            let v = (mono[i] + mono[i + 1] + mono[i + 2]) / 3.0;
            out.push(v);
            i += 3;
        }
        return Ok(out);
    }

    // Generic linear-interpolation resample to 16 kHz for any other input rate.
    let out_len = ((mono.len() as f64) * 16_000.0 / sample_rate as f64) as usize;
    let mut out = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let src_pos = (n as f64) * ratio;
        let i0 = src_pos.floor() as usize;
        let i1 = (i0 + 1).min(mono.len() - 1);
        let frac = (src_pos - i0 as f64) as f32;
        out.push(mono[i0] * (1.0 - frac) + mono[i1] * frac);
    }
    mono.clear();
    mono.shrink_to_fit();
    Ok(out)
}

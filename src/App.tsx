import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import "./App.css";

type RecordingStarted = {
  path: string;
  sample_rate: number;
  channels: number;
  input_device: string;
  system_audio: string;
};

type RecordingStopped = {
  path: string;
  duration_ms: number;
};

type MeterEvent = {
  mic_peak: number;
  mic_rms: number;
  system_peak: number;
  system_rms: number;
  elapsed_ms: number;
};

type TranscriptResult = {
  text_path: string;
  text: string;
  language: string;
  segment_count: number;
};

type TranscriptSegment = {
  path: string;
  index: number;
  start: number;
  end: number;
  text: string;
};

type TranscriptDone = {
  path: string;
  language: string;
  segment_count: number;
};

type Lang = "auto" | "es" | "en";
const LANG_LABEL: Record<Lang, string> = { auto: "AUTO", es: "ES", en: "EN" };

type RecordingEntry = {
  path: string;
  name: string;
  duration_ms: number;
  size_bytes: number;
  modified_secs: number;
  has_transcript: boolean;
};

type Drawer = "closed" | "files" | "transcript" | "setup";

type ModelStatus = {
  id: string;
  label: string;
  file: string;
  bytes: number;
  downloaded: boolean;
  local_bytes: number;
};

type SetupStatus = {
  mic_permission: string;
  screen_permission: string;
  models_dir: string;
  models: ModelStatus[];
};

type DownloadProgress = {
  model: string;
  downloaded: number;
  total: number;
};
type RecState = "idle" | "rec" | "paused";
type ToastKind = "error" | "info" | "ok";
type Toast = { id: number; message: string; kind: ToastKind };

const WINDOW_CLOSED = { width: 472, height: 240 };
const WINDOW_OPEN = { width: 472, height: 700 };
const WAVE_ROWS = 3;
const RAMP = ["·", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

function App() {
  const [recState, setRecState] = useState<RecState>("idle");
  const [recording, setRecording] = useState<RecordingStarted | null>(null);
  const [lastFile, setLastFile] = useState<string>("");
  const [activeFile, setActiveFile] = useState<RecordingEntry | null>(null);
  const [files, setFiles] = useState<RecordingEntry[]>([]);
  const [drawer, setDrawer] = useState<Drawer>("closed");
  const [transcript, setTranscript] = useState("");
  const [segments, setSegments] = useState<TranscriptSegment[]>([]);
  const [isTranscribing, setIsTranscribing] = useState(false);
  const [lang, setLang] = useState<Lang>("es");
  const [detectedLang, setDetectedLang] = useState("");
  const transcribingForRef = useRef<string>("");
  const [toasts, setToasts] = useState<Toast[]>([]);
  const toastIdRef = useRef(0);
  const pushToast = (message: string, kind: ToastKind = "error") => {
    const id = ++toastIdRef.current;
    setToasts((prev) => [...prev, { id, message, kind }]);
    window.setTimeout(() => {
      setToasts((prev) => prev.filter((t) => t.id !== id));
    }, 4500);
  };
  const setError = (message: string) => {
    if (message) pushToast(message, "error");
  };
  const [elapsedMs, setElapsedMs] = useState(0);
  const [micOn, setMicOn] = useState(true);
  const [sysOn, setSysOn] = useState(true);
  const [aecOn, setAecOn] = useState(true);
  const [isPinned, setIsPinned] = useState(false);
  const [setup, setSetup] = useState<SetupStatus | null>(null);
  const [activeModel, setActiveModel] = useState<string>("small");
  const [downloading, setDownloading] = useState<string>("");
  const [downloadProgress, setDownloadProgress] = useState<DownloadProgress | null>(null);
  const [transcribeProgress, setTranscribeProgress] = useState<number>(-1);
  const [meter, setMeter] = useState<MeterEvent>({
    mic_peak: 0,
    mic_rms: 0,
    system_peak: 0,
    system_rms: 0,
    elapsed_ms: 0,
  });

  const [waveT, setWaveT] = useState(0);
  const [waveCols, setWaveCols] = useState(48);
  const waveRef = useRef<HTMLPreElement | null>(null);
  const ampRef = useRef(0.06);
  const transcriptBodyRef = useRef<HTMLDivElement | null>(null);

  const tf = useMemo(() => formatTime(elapsedMs), [elapsedMs]);
  const stateClass =
    recState === "rec"
      ? "state-rec"
      : recState === "paused"
        ? "state-paused"
        : isTranscribing
          ? "state-transcribing"
          : "state-idle";
  const stateText = isTranscribing
    ? transcribeProgress > 0
      ? `TRANSCRIBING ${transcribeProgress}%`
      : "TRANSCRIBING"
    : recState === "rec"
      ? "RECORDING"
      : recState === "paused"
        ? "PAUSED"
        : lastFile
          ? "READY"
          : "IDLE";
  const liveSession =
    activeFile === null && (recState === "rec" || recState === "paused");

  // listen to streaming transcript segments
  useEffect(() => {
    const dispose = listen<TranscriptSegment>("transcript-segment", (event) => {
      const seg = event.payload;
      if (transcribingForRef.current && seg.path !== transcribingForRef.current) return;
      setSegments((prev) => [...prev, seg]);
    });
    return () => {
      dispose.then((fn) => fn());
    };
  }, []);

  useEffect(() => {
    const dispose = listen<TranscriptDone>("transcript-done", (event) => {
      const payload = event.payload;
      if (transcribingForRef.current && payload.path !== transcribingForRef.current) return;
      setDetectedLang(payload.language || "");
      setTranscribeProgress(100);
    });
    return () => {
      dispose.then((fn) => fn());
    };
  }, []);

  useEffect(() => {
    const dispose = listen<{ path: string; percent: number }>(
      "transcript-progress",
      (event) => {
        const { path, percent } = event.payload;
        if (transcribingForRef.current && path !== transcribingForRef.current) return;
        setTranscribeProgress(percent);
      },
    );
    return () => {
      dispose.then((fn) => fn());
    };
  }, []);

  useEffect(() => {
    const dispose = listen<{ path: string; model: string; language: string }>(
      "transcript-started",
      (event) => {
        const { path, model, language } = event.payload;
        if (transcribingForRef.current && path !== transcribingForRef.current) return;
        setTranscribeProgress(0);
        pushToast(`whisper · ${model} · ${language.toUpperCase()} running`, "info");
      },
    );
    return () => {
      dispose.then((fn) => fn());
    };
  }, []);

  // listen to audio meter events
  useEffect(() => {
    const unlisten = listen<MeterEvent>("audio-meter", (event) => {
      const payload = event.payload;
      setMeter((previous) => ({
        mic_peak: Math.max(payload.mic_peak, previous.mic_peak * 0.82),
        mic_rms: Math.max(payload.mic_rms, previous.mic_rms * 0.76),
        system_peak: Math.max(payload.system_peak, previous.system_peak * 0.82),
        system_rms: Math.max(payload.system_rms, previous.system_rms * 0.76),
        elapsed_ms: payload.elapsed_ms,
      }));
      setElapsedMs(payload.elapsed_ms);
    });
    return () => {
      unlisten.then((dispose) => dispose());
    };
  }, []);

  // load files + setup on mount
  useEffect(() => {
    void refreshFiles();
    void refreshSetup();
  }, []);

  // download progress events
  useEffect(() => {
    const dispose = listen<DownloadProgress>("model-download-progress", (event) => {
      setDownloadProgress(event.payload);
    });
    return () => {
      dispose.then((fn) => fn());
    };
  }, []);

  // resize window when drawer toggles
  useEffect(() => {
    const target = drawer === "closed" ? WINDOW_CLOSED : WINDOW_OPEN;
    getCurrentWindow()
      .setSize(new LogicalSize(target.width, target.height))
      .catch(() => undefined);
  }, [drawer]);

  // waveform amplitude drives off mic+sys meters
  useEffect(() => {
    const target = Math.max(
      micOn ? meter.mic_rms : 0,
      sysOn ? meter.system_rms : 0,
    );
    ampRef.current += (Math.min(1, target * 1.8) - ampRef.current) * 0.35;
  }, [meter, micOn, sysOn]);

  // waveform animation clock
  useEffect(() => {
    let raf = 0;
    let last = performance.now();
    const tick = (now: number) => {
      const dt = Math.min(100, now - last) / 1000;
      last = now;
      setWaveT((t) => t + dt * (recState === "rec" ? 1 : 0.15));
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [recState]);

  // measure wave columns
  useEffect(() => {
    if (!waveRef.current) return;
    const measure = () => {
      const el = waveRef.current;
      if (!el) return;
      const probe = document.createElement("span");
      probe.style.font = getComputedStyle(el).font;
      probe.style.visibility = "hidden";
      probe.style.position = "absolute";
      probe.textContent = "M".repeat(40);
      document.body.appendChild(probe);
      const charW = probe.getBoundingClientRect().width / 40;
      document.body.removeChild(probe);
      const w = el.clientWidth;
      setWaveCols(Math.max(20, Math.floor(w / charW) - 1));
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(waveRef.current);
    window.addEventListener("resize", measure);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", measure);
    };
  }, []);

  const waveStr = useMemo(() => {
    return renderWave(genCols(waveT, waveCols, ampRef.current), WAVE_ROWS);
  }, [waveT, waveCols]);

  // auto-scroll transcript when new segments arrive
  useEffect(() => {
    const el = transcriptBodyRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [segments.length, drawer]);

  async function refreshFiles() {
    try {
      const list = await invoke<RecordingEntry[]>("list_recordings");
      setFiles(list);
    } catch (err) {
      setError(String(err));
    }
  }

  async function refreshSetup() {
    try {
      const status = await invoke<SetupStatus>("setup_status");
      setSetup(status);
    } catch (err) {
      setError(String(err));
    }
  }

  async function startModelDownload(modelId: string) {
    setDownloading(modelId);
    setDownloadProgress({ model: modelId, downloaded: 0, total: 0 });
    try {
      await invoke("download_model", { modelId });
      pushToast(`model "${modelId}" downloaded`, "ok");
      await refreshSetup();
    } catch (err) {
      setError(String(err));
    } finally {
      setDownloading("");
      setDownloadProgress(null);
    }
  }

  async function cancelModelDownload() {
    try {
      await invoke("cancel_model_download");
    } catch (err) {
      setError(String(err));
    }
  }

  async function openRecordingsDir() {
    try {
      await invoke("open_recordings_dir");
    } catch (err) {
      setError(String(err));
    }
  }

  async function openPrivacy(panel: "screen" | "microphone") {
    try {
      await invoke("open_privacy_settings", { panel });
    } catch (err) {
      setError(String(err));
    }
  }

  async function resetPermissions() {
    try {
      await invoke("reset_permissions");
      pushToast("permissions reset · restarting…", "ok");
      window.setTimeout(() => {
        void invoke("restart_app");
      }, 900);
    } catch (err) {
      setError(String(err));
    }
  }

  async function testPermission(kind: "mic" | "screen") {
    try {
      const cmd = kind === "mic" ? "request_mic_permission" : "request_screen_permission";
      const result = await invoke<string>(cmd);
      const label = kind === "mic" ? "microphone" : "screen recording";
      if (result === "authorized") {
        pushToast(`${label} · authorized`, "ok");
      } else {
        pushToast(`${label} · ${result}`, "error");
      }
      void refreshSetup();
    } catch (err) {
      setError(String(err));
    }
  }

  async function revealRecording(path: string, e?: { stopPropagation: () => void }) {
    if (e) e.stopPropagation();
    try {
      await invoke("reveal_recording", { path });
    } catch (err) {
      setError(String(err));
    }
  }

  async function startRec() {
    if (recState !== "idle") return;
    setError("");
    setTranscript("");
    setSegments([]);
    setDetectedLang("");
    setActiveFile(null);
    try {
      const started = await invoke<RecordingStarted>("start_recording");
      setRecording(started);
      setLastFile(started.path);
      setElapsedMs(0);
      setRecState("rec");
    } catch (err) {
      setError(String(err));
    }
  }

  async function stopRec() {
    if (recState === "idle") return;
    try {
      const stopped = await invoke<RecordingStopped>("stop_recording");
      setRecState("idle");
      setLastFile(stopped.path);
      setElapsedMs(stopped.duration_ms);
      void refreshFiles();
      setDrawer("transcript");
      void runTranscribe({ path: stopped.path } as RecordingEntry);
    } catch (err) {
      setError(String(err));
    }
  }

  async function togglePause() {
    if (recState === "rec") {
      try {
        await invoke("pause_recording");
        setRecState("paused");
      } catch (err) {
        setError(String(err));
      }
    } else if (recState === "paused") {
      try {
        await invoke("resume_recording");
        setRecState("rec");
      } catch (err) {
        setError(String(err));
      }
    }
  }

  async function runTranscribe(
    forFile?: RecordingEntry | null,
    overrideLang?: Lang,
  ) {
    const targetPath = forFile?.path ?? lastFile;
    if (!targetPath || isTranscribing) return;
    setError("");
    setIsTranscribing(true);
    setSegments([]);
    setTranscript("");
    setDetectedLang("");
    setTranscribeProgress(0);
    transcribingForRef.current = targetPath;
    const useLang = overrideLang ?? lang;
    try {
      const result = await invoke<TranscriptResult>("transcribe_recording", {
        path: targetPath,
        model: activeModel,
        language: useLang,
      });
      setTranscript(result.text);
      setDetectedLang(result.language || "");
      void refreshFiles();
    } catch (err) {
      setError(String(err));
    } finally {
      setIsTranscribing(false);
      transcribingForRef.current = "";
      setTranscribeProgress(-1);
    }
  }

  async function openFile(entry: RecordingEntry) {
    setActiveFile(entry);
    setDrawer("transcript");
    setSegments([]);
    setDetectedLang("");
    try {
      const text = await invoke<string>("read_transcript", { path: entry.path });
      setTranscript(text);
    } catch (err) {
      setError(String(err));
    }
  }

  function pickLang(next: Lang, autoRerun = false) {
    setLang(next);
    if (autoRerun && (lastFile || activeFile) && !isTranscribing) {
      void runTranscribe(activeFile, next);
    }
  }

  function openLiveTranscript() {
    setActiveFile(null);
    setDrawer("transcript");
  }

  function toggleDrawer(target: Drawer) {
    setDrawer((current) => (current === target ? "closed" : target));
  }

  async function togglePinned() {
    const next = !isPinned;
    setIsPinned(next);
    try {
      await getCurrentWindow().setAlwaysOnTop(next);
    } catch (err) {
      setError(String(err));
    }
  }

  async function pushGates(mic: boolean, system: boolean) {
    try {
      await invoke("set_mix_gates", { mic, system });
    } catch (err) {
      setError(String(err));
    }
  }

  async function toggleAec() {
    const next = !aecOn;
    setAecOn(next);
    try {
      await invoke("set_aec_enabled", { enabled: next });
    } catch (err) {
      setError(String(err));
    }
  }

  function toggleMic() {
    const next = !micOn;
    setMicOn(next);
    void pushGates(next, sysOn);
  }

  function toggleSys() {
    const next = !sysOn;
    setSysOn(next);
    void pushGates(micOn, next);
  }

  async function copyTranscript() {
    const text = transcript || segments.map((s) => s.text).join("\n");
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
      pushToast("transcript copied", "ok");
    } catch (err) {
      setError(String(err));
    }
  }

  // keyboard shortcuts
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA")) return;
      if (e.key === " ") {
        e.preventDefault();
        if (recState === "idle") void startRec();
        else void togglePause();
      } else if (e.key === "r" || e.key === "R") {
        e.preventDefault();
        if (recState === "idle") void startRec();
        else void stopRec();
      } else if (e.key === "p" || e.key === "P") {
        if (recState !== "idle") {
          e.preventDefault();
          void togglePause();
        }
      } else if (e.key === "Escape" || e.key === "s" || e.key === "S") {
        if (recState !== "idle") {
          e.preventDefault();
          void stopRec();
        }
      } else if (e.key === "f" || e.key === "F") {
        e.preventDefault();
        toggleDrawer("files");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [recState]);

  const showLastInList = lastFile && !files.some((f) => f.path === lastFile);

  const transcriptName = activeFile?.name ?? lastBaseName(lastFile) ?? "no-session.wav";
  const transcriptDur = activeFile
    ? formatDurMs(activeFile.duration_ms)
    : tf.ms;

  return (
    <div className="stage">
      <div className={`device ${stateClass}`}>
        <div className="topbar" data-tauri-drag-region>
          <div className="brand-group" data-tauri-drag-region>
            <div className="brand" data-tauri-drag-region>
              REKK<small>mini · m-88</small>
            </div>
            <span className="state-pill" data-tauri-drag-region>
              <span className="dot" />
              {stateText}
            </span>
          </div>
          <div className="top-controls">
            <button
              type="button"
              className={`pin-btn${drawer === "setup" ? " on" : ""}`}
              onClick={() => {
                if (drawer === "setup") setDrawer("closed");
                else {
                  void refreshSetup();
                  setDrawer("setup");
                }
              }}
              title="Setup"
            >
              setup
            </button>
            <button
              type="button"
              className={`pin-btn${aecOn ? " on" : ""}`}
              onClick={toggleAec}
              title={
                aecOn
                  ? "AEC on — echo cancelled. Click to disable (use if you wear headphones)."
                  : "AEC off — mic and speakers will echo. Click to re-enable."
              }
            >
              {aecOn ? "aec" : "aec✗"}
            </button>
            <button
              type="button"
              className={`pin-btn${isPinned ? " on" : ""}`}
              onClick={togglePinned}
              title={isPinned ? "Unpin window" : "Pin on top"}
            >
              {isPinned ? "pinned" : "pin"}
            </button>
            <button
              type="button"
              className="close-btn"
              onClick={() => getCurrentWindow().close()}
              title="Close window"
            >
              ×
            </button>
          </div>
        </div>

        <div className="lcd">
          {isTranscribing && (
            <div className="lcd-tx-bar">
              <div
                className="lcd-tx-bar-fill"
                style={{
                  width:
                    transcribeProgress >= 0 ? `${transcribeProgress}%` : "100%",
                  animation:
                    transcribeProgress < 0
                      ? "lcd-tx-pulse 1.4s ease-in-out infinite"
                      : undefined,
                }}
              />
            </div>
          )}
          <div className="lcd-time">
            {tf.ms}
            <span className="ms">.{tf.tenth}</span>
          </div>
          <div className="lcd-rate">
            {recording?.sample_rate ? `${(recording.sample_rate / 1000).toFixed(0)}k` : "48k"} · MIX MONO
          </div>
          <div className="lcd-file" title={lastFile || "no recording yet"}>
            {lastBaseName(lastFile) ?? "—"}
          </div>

          <div className="lcd-wave">
            <pre ref={waveRef}>{waveStr}</pre>
          </div>

          <Meter
            kind="mic"
            value={meter.mic_rms}
            enabled={micOn}
            onToggle={toggleMic}
            row="mic"
          />
          <Meter
            kind="sys"
            value={meter.system_rms}
            enabled={sysOn}
            onToggle={toggleSys}
            row="sys"
          />
        </div>

        <div className="controls">
          <button
            className={`ctl${recState === "paused" ? " armed-pause" : ""}`}
            onClick={togglePause}
            title="pause / resume · P"
            disabled={recState === "idle"}
          >
            {recState === "paused" ? "▶" : "❚❚"}
          </button>
          <button
            className="ctl"
            onClick={stopRec}
            title="stop · S"
            disabled={recState === "idle"}
          >
            ■
          </button>
          <button
            className={`ctl rec${recState === "rec" ? " armed" : ""}${recState === "paused" ? " armed-pause" : ""}`}
            onClick={() => (recState === "idle" ? startRec() : stopRec())}
            title="record · R"
          >
            ●
          </button>
          <span className="ctl-sep" />
          <button
            className={`ctl files${drawer !== "closed" ? " on" : ""}`}
            onClick={() => toggleDrawer("files")}
            title="files · F"
          >
            ☰
          </button>
        </div>

        <div className={`drawer${drawer !== "closed" ? " open" : ""}`}>
          <div className="drawer-inner">
            <div className="drawer-head">
              <div className="crumbs">
                {drawer === "files" && (
                  <span>
                    RECORDINGS · {files.length + (showLastInList ? 1 : 0)}
                  </span>
                )}
                {drawer === "transcript" && (
                  <>
                    <button onClick={() => setDrawer("files")}>◀ FILES</button>
                    <span className="sep">/</span>
                    <span>{activeFile ? "TRANSCRIPT" : "LIVE"}</span>
                  </>
                )}
                {drawer === "setup" && <span>SETUP · DIAGNOSTICS</span>}
              </div>
              <div style={{ display: "flex", gap: 6, alignItems: "center" }}>
                {drawer === "files" && (
                  <button
                    type="button"
                    className="head-btn"
                    onClick={openRecordingsDir}
                    title="open recordings folder in Finder"
                  >
                    FINDER ↗
                  </button>
                )}
                {drawer === "transcript" && activeFile && (
                  <button
                    type="button"
                    className="head-btn"
                    onClick={() => revealRecording(activeFile.path)}
                    title="reveal in Finder"
                  >
                    REVEAL ↗
                  </button>
                )}
                <button className="close" onClick={() => setDrawer("closed")} title="close">
                  ×
                </button>
              </div>
            </div>

            {drawer === "files" && (
              <>
                <div className="files-head">
                  <span>date</span>
                  <span>file</span>
                  <span style={{ textAlign: "right" }}>dur</span>
                  <span />
                </div>
                <div className="files-list">
                  {recState === "rec" && (
                    <div
                      className="file-row"
                      onClick={openLiveTranscript}
                      style={{ background: "rgba(125,255,154,.04)" }}
                    >
                      <span className="date" style={{ color: "var(--lcd-fg)" }}>
                        LIVE
                      </span>
                      <span className="name">current-session.wav</span>
                      <span className="dur">{tf.ms}</span>
                      <span className="go">↗</span>
                    </div>
                  )}
                  {files.length === 0 && recState !== "rec" && (
                    <div className="files-empty">no recordings yet — hit ● to start</div>
                  )}
                  {files.map((f) => (
                    <div
                      key={f.path}
                      className={`file-row${activeFile?.path === f.path ? " active" : ""}`}
                      onClick={() => openFile(f)}
                    >
                      <span className="date">{formatDate(f.modified_secs)}</span>
                      <span className="name">{f.name}</span>
                      <span className="dur">{formatDurMs(f.duration_ms)}</span>
                      <button
                        type="button"
                        className="go"
                        onClick={(e) => revealRecording(f.path, e)}
                        title="reveal in Finder"
                      >
                        ↗
                      </button>
                    </div>
                  ))}
                </div>
              </>
            )}

            {drawer === "setup" && (
              <div className="setup-view">
                <div className="setup-section">
                  <span className="setup-section-title">PERMISSIONS</span>
                  <div className="setup-row">
                    <span className="setup-key">microphone</span>
                    <span className={`setup-val ${permClass(setup?.mic_permission)}`}>
                      {setup?.mic_permission ?? "—"}
                    </span>
                    <span className="setup-row-actions-inline">
                      <button
                        type="button"
                        className="setup-btn"
                        onClick={() => testPermission("mic")}
                        title="trigger the macOS microphone prompt"
                      >
                        ▶ test
                      </button>
                      <button
                        type="button"
                        className="setup-btn"
                        onClick={() => openPrivacy("microphone")}
                        title="open Microphone settings"
                      >
                        open ↗
                      </button>
                    </span>
                  </div>
                  <div className="setup-row">
                    <span className="setup-key">screen + system audio</span>
                    <span className={`setup-val ${permClass(setup?.screen_permission)}`}>
                      {setup?.screen_permission ?? "—"}
                    </span>
                    <span className="setup-row-actions-inline">
                      <button
                        type="button"
                        className="setup-btn"
                        onClick={() => testPermission("screen")}
                        title="trigger the macOS screen recording prompt"
                      >
                        ▶ test
                      </button>
                      <button
                        type="button"
                        className="setup-btn"
                        onClick={() => openPrivacy("screen")}
                        title="open Screen Recording settings"
                      >
                        open ↗
                      </button>
                    </span>
                  </div>
                  <div className="setup-row setup-row-actions">
                    <button
                      type="button"
                      className="setup-btn danger"
                      onClick={resetPermissions}
                      title="run tccutil reset for com.dbr.rek and restart"
                    >
                      ⟳ reset & restart
                    </button>
                    <span className="setup-hint">
                      app must restart so macOS state isn't cached as stale
                    </span>
                  </div>
                </div>

                <div className="setup-section">
                  <span className="setup-section-title">WHISPER MODELS</span>
                  <div className="setup-models">
                    {(setup?.models ?? []).map((m) => {
                      const isActive = activeModel === m.id;
                      const isDownloading = downloading === m.id;
                      const pct =
                        downloadProgress?.model === m.id && downloadProgress.total > 0
                          ? Math.floor(
                              (downloadProgress.downloaded / downloadProgress.total) * 100,
                            )
                          : 0;
                      return (
                        <div
                          key={m.id}
                          className={`model-row${isActive ? " active" : ""}`}
                        >
                          <button
                            type="button"
                            className="model-select"
                            disabled={!m.downloaded}
                            onClick={() => setActiveModel(m.id)}
                            title={
                              m.downloaded
                                ? "Use this model"
                                : "Download to use this model"
                            }
                          >
                            {isActive ? "●" : "○"}
                          </button>
                          <span className="model-name">{m.label}</span>
                          <span className="model-size">{fmtMB(m.bytes)}</span>
                          {isDownloading ? (
                            <>
                              <div className="model-progress">
                                <div
                                  className="model-progress-bar"
                                  style={{ width: `${pct}%` }}
                                />
                                <span className="model-progress-text">{pct}%</span>
                              </div>
                              <button
                                type="button"
                                className="model-btn"
                                onClick={cancelModelDownload}
                              >
                                cancel
                              </button>
                            </>
                          ) : m.downloaded ? (
                            <span className="model-status ok">ready</span>
                          ) : (
                            <button
                              type="button"
                              className="model-btn"
                              onClick={() => startModelDownload(m.id)}
                              disabled={Boolean(downloading)}
                            >
                              download
                            </button>
                          )}
                        </div>
                      );
                    })}
                  </div>
                </div>

                <div className="setup-section">
                  <span className="setup-section-title">PATHS</span>
                  <div className="setup-row">
                    <span className="setup-key">models dir</span>
                    <span className="setup-val small">{setup?.models_dir ?? "—"}</span>
                  </div>
                </div>
              </div>
            )}

            {drawer === "transcript" && (
              <div className="transcript-view">
                <div className="transcript-meta">
                  <span>
                    ► <b>{transcriptName}</b>
                  </span>
                  <span>
                    {transcriptDur} · whisper · {activeModel}
                    <span className="lang-group">
                      {(["es", "en", "auto"] as Lang[]).map((l) => (
                        <button
                          key={l}
                          type="button"
                          className={`lang-pill${lang === l ? " on" : ""}`}
                          onClick={() => pickLang(l, true)}
                          disabled={isTranscribing}
                          title={`set language to ${LANG_LABEL[l]} and re-run`}
                        >
                          {LANG_LABEL[l]}
                        </button>
                      ))}
                    </span>
                    {detectedLang && lang === "auto" && (
                      <span className="lang-detected" title="detected language">
                        · {detectedLang}
                      </span>
                    )}
                  </span>
                </div>

                {segments.length > 0 ? (
                  <div className="transcript-body" ref={transcriptBodyRef}>
                    {segments.map((seg) => (
                      <div key={seg.index} className="t-line">
                        <span className="ts">{fmtSecs(seg.start)}</span>
                        <span className="text">{seg.text}</span>
                      </div>
                    ))}
                    {isTranscribing && (
                      <div className="t-line">
                        <span className="ts">…</span>
                        <span className="text partial">·</span>
                      </div>
                    )}
                  </div>
                ) : transcript ? (
                  <div className="transcript-body">
                    {transcript.split("\n").map((line, i) =>
                      line.trim() ? (
                        <div key={i} className="t-line">
                          <span className="ts">{String(i + 1).padStart(2, "0")}</span>
                          <span className="text">{line}</span>
                        </div>
                      ) : null,
                    )}
                  </div>
                ) : (
                  <div className="transcript-empty">
                    {liveSession ? (
                      <>recording in progress — transcript will run on stop.</>
                    ) : isTranscribing ? (
                      <>loading whisper {LANG_LABEL[lang]}…</>
                    ) : (
                      <>
                        no transcript yet for this session.
                        <br />
                        <button
                          onClick={() => runTranscribe(activeFile)}
                          disabled={(!lastFile && !activeFile) || isTranscribing}
                        >
                          ▶ RUN WHISPER {LANG_LABEL[lang]}
                        </button>
                      </>
                    )}
                  </div>
                )}

                <div className="transcript-foot">
                  <span className="t-status">
                    {isTranscribing ? (
                      <>
                        <span className="dot" />
                        TRANSCRIBING · {segments.length} seg
                      </>
                    ) : transcript || segments.length > 0 ? (
                      <>● {segments.length || lineCount(transcript)} seg</>
                    ) : liveSession ? (
                      <>
                        <span className="dot" />
                        STREAMING
                      </>
                    ) : (
                      <>○ EMPTY</>
                    )}
                  </span>
                  <span className="t-actions">
                    <button
                      onClick={() => runTranscribe(activeFile)}
                      disabled={isTranscribing || (!lastFile && !activeFile)}
                      title={`re-transcribe with ${LANG_LABEL[lang]} / ${activeModel}`}
                    >
                      ↻ REDO
                    </button>
                    <button onClick={copyTranscript} disabled={!transcript && segments.length === 0}>
                      COPY
                    </button>
                  </span>
                </div>
              </div>
            )}
          </div>
        </div>

        <div className="device-foot">
          <span>
            <b>SPACE</b> rec/pause · <b>S</b> stop · <b>F</b> files
          </span>
          <span>48k · 16bit</span>
        </div>

        <div className="toast-stack">
          {toasts.map((t) => (
            <div key={t.id} className={`toast toast-${t.kind}`}>
              <span className="toast-kind">
                {t.kind === "error" ? "ERR" : t.kind === "ok" ? "OK" : "i"}
              </span>
              <span className="toast-msg">{t.message}</span>
              <button
                type="button"
                className="toast-x"
                onClick={() =>
                  setToasts((prev) => prev.filter((x) => x.id !== t.id))
                }
                title="dismiss"
              >
                ×
              </button>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

function Meter({
  kind,
  value,
  enabled,
  onToggle,
  row,
}: {
  kind: string;
  value: number;
  enabled: boolean;
  onToggle: () => void;
  row: string;
}) {
  const total = 20;
  const effective = enabled ? value : 0;
  const lit = Math.round(Math.min(effective * 1.9, 1) * total);

  return (
    <div className={`lcd-meter ${row}${enabled ? "" : " muted"}`}>
      <button type="button" className="ch" onClick={onToggle} title={`${enabled ? "Mute" : "Unmute"} ${kind}`}>
        {kind === "mic" ? "M" : "S"}
      </button>
      <span className="bars">
        {Array.from({ length: total }, (_, index) => {
          let cls = "";
          let ch = "░";
          if (index < lit) {
            ch = "█";
            cls = index > total * 0.85 ? "hot" : index > total * 0.6 ? "mid" : "on";
          }
          return (
            <span className={cls} key={index}>
              {ch}
            </span>
          );
        })}
      </span>
      <span className="db">{enabled ? toDb(value) : "MUTE"}</span>
    </div>
  );
}

function permClass(perm: string | undefined) {
  if (perm === "authorized") return "ok";
  if (perm === "denied" || perm === "restricted") return "bad";
  return "neutral";
}

function fmtMB(bytes: number) {
  if (bytes >= 1_000_000_000) return `${(bytes / 1_000_000_000).toFixed(1)} GB`;
  return `${Math.round(bytes / 1_000_000)} MB`;
}

function fmtSecs(secs: number) {
  if (!Number.isFinite(secs) || secs < 0) return "00:00";
  const total = Math.floor(secs);
  const m = Math.floor(total / 60).toString().padStart(2, "0");
  const s = (total % 60).toString().padStart(2, "0");
  return `${m}:${s}`;
}

function lineCount(text: string) {
  if (!text) return 0;
  return text.split("\n").filter((line) => line.trim().length > 0).length;
}

function formatTime(ms: number) {
  const total = Math.floor(ms / 1000);
  const minutes = Math.floor(total / 60)
    .toString()
    .padStart(2, "0");
  const seconds = (total % 60).toString().padStart(2, "0");
  const tenth = Math.floor((ms % 1000) / 100);
  return { ms: `${minutes}:${seconds}`, tenth };
}

function formatDurMs(ms: number) {
  if (!ms || ms < 1000) return "—";
  const total = Math.floor(ms / 1000);
  const minutes = Math.floor(total / 60)
    .toString()
    .padStart(2, "0");
  const seconds = (total % 60).toString().padStart(2, "0");
  return `${minutes}:${seconds}`;
}

function formatDate(secs: number) {
  if (!secs) return "--";
  const d = new Date(secs * 1000);
  return `${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}

function toDb(value: number) {
  if (value < 0.001) return "-∞";
  return `${Math.round(20 * Math.log10(value))}dB`;
}

function lastBaseName(path: string): string | null {
  if (!path) return null;
  return path.split("/").pop() ?? null;
}

const SIN = Math.sin;

function genCols(t: number, n: number, amp: number) {
  const cols: number[] = new Array(n);
  for (let i = 0; i < n; i += 1) {
    const x = i + t * 60;
    const a =
      SIN(x * 0.21) * 0.55 + SIN(x * 0.07 + t * 1.3) * 0.35 + SIN(x * 0.43 + t * 2.1) * 0.2;
    const noise = (SIN(x * 3.1 + t * 7) + SIN(x * 5.9 + t * 11)) * 0.08;
    let v = Math.abs(a + noise) * amp;
    if (v > 1) v = 1;
    cols[i] = Math.round(v * (RAMP.length - 1));
  }
  return cols;
}

function renderWave(cols: number[], rows: number) {
  const max = RAMP.length - 1;
  const half = Math.floor(rows / 2);
  const out: string[] = [];
  for (let r = 0; r < rows; r += 1) {
    let line = "";
    const dist = Math.abs(r - half) / Math.max(1, half);
    for (let c = 0; c < cols.length; c += 1) {
      const norm = cols[c] / max;
      if (norm >= dist - 0.02) {
        const span = norm - dist;
        line += RAMP[Math.max(0, Math.min(max, Math.round(span * max * 1.4)))];
      } else {
        line += " ";
      }
    }
    out.push(line);
  }
  return out.join("\n");
}

export default App;

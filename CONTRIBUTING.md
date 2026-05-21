# Contributing to Rekk

Thanks for taking the time. Rekk is a small macOS audio recorder + local Whisper transcriber built with Tauri + Rust + React. Contributions of any size are welcome.

## Reporting bugs

Open an issue on GitHub with:

- macOS version (`sw_vers -productVersion`).
- Hardware (Apple Silicon / Intel).
- The Rekk version (visible inside the app or in `Cargo.toml`).
- Exact steps to reproduce.
- The error toast text or the binary's stderr from `Rekk.app/Contents/MacOS/tauri-apprek` if it crashes at launch.

## Suggesting features

Open an issue before writing code. Rekk has an intentionally small surface (record → mix → transcribe locally), so a tight discussion up front avoids wasted work.

## Development setup

Prerequisites — install all of these via Homebrew on macOS:

```sh
brew install cmake meson ninja
```

Then:

```sh
git clone https://github.com/danibram/rekk
cd rekk
npm install
npm run tauri dev
```

The first build compiles `whisper.cpp` and `webrtc-audio-processing` from source — expect 2–4 minutes once. Subsequent builds are incremental.

### Required Xcode toolchain

- Xcode 15 or newer (for the Swift toolchain that `screencapturekit-rs` needs).
- macOS SDK 13.0+ (Xcode bundles a recent one).

The `MACOSX_DEPLOYMENT_TARGET` is pinned to 11.0 in `src-tauri/.cargo/config.toml` because `whisper.cpp` uses `std::filesystem` which requires 10.15+. Don't lower this without checking that whisper.cpp still builds.

## Running tests

There are no automated tests yet. Manually verify:

1. Granted Microphone + Screen Recording permission in System Settings.
2. Record a clip (record, pause, resume, stop).
3. The WAV lands in `~/Music/Rekk/` (or `~/Documents/Rekk/` as fallback).
4. After stop, the transcript drawer auto-opens and Whisper segments stream in.
5. Mute toggles `M` / `S` in the LCD remove that source from the mix.
6. `AEC` toggle pill removes the speaker→mic echo when recording with speakers.

## Style

- Rust: `cargo fmt` before committing.
- TypeScript: oxlint / no autoformatter is enforced, but try to follow the existing style.
- No comments unless the *why* is non-obvious — names should explain *what*.
- Keep CSS in `src/App.css`. Tailwind / styled-components are explicitly avoided.

## Commit messages

Short imperative subject. Optionally a body explaining *why*, not *what*.

```
add ducking fallback when AEC fails to init

WebRTC APM bombs on devices reporting a 44.1kHz mic config because
SCK won't downsample below 48k. Fall back to gating mic when system
is loud so the user isn't left with raw echo.
```

## License

By contributing you agree your changes are released under the [MIT License](./LICENSE) that governs the rest of the project.

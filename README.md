# zoomcap-rs

hacking together a small auto-zoom screen recorder in rust + tauri v2 because I didn't feel like paying $89 for screen studio, still very wip

## what works

- macOS `ScreenCaptureKit` grabs raw BGRA frames, pushes them into a bounded `mpsc::sync_channel(8)` (`try_send` drops frames instead of stalling the capture callback), and a writer thread pipes straight into `ffmpeg` `stdin` (`h264_videotoolbox`) — completely bypassing Tauri IPC so we aren't shoveling raw frames through the webview
- click-triggered zoom/pan keyframes compiled into piecewise `ffmpeg` `crop` + `scale` (`lanczos`) filter expressions using cubic ease-in-out transitions
- basic `start_recording` / `stop_recording` Tauri commands wired up on the Rust side

## broken / todo

- no audio yet — still need to wire mic + system audio capture and mux it without A/V drift
- frame timestamps drift a bit when CPU spikes past ~80% since `ffmpeg` is fed constant-fps `rawvideo` instead of real PTS timestamps
- Windows Graphics Capture (`WGC`) backend not started yet (just a commented-out `windows` crate placeholder in `src-tauri/Cargo.toml`)
- no frontend UI or timeline preview yet — just backend prototyping for now

## notes

- targets macOS 12.3+ (needs Screen Recording permission in System Settings)
- expects `ffmpeg` on `PATH` built with `h264_videotoolbox`
- outputs `.mp4` with `+faststart` once `stop_recording` drops the stream and closes `ffmpeg` `stdin`

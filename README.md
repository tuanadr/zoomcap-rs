# zoomcap-rs

testing auto-zoom screen recording in rust + tauri (trying to avoid paying $89 for screen studio lol)

currently working:
- grabbing raw bgra frames via screencapturekit and piping straight into ffmpeg stdin
- basic cubic ease-in-out crop math from click coords

broken / todo:
- audio capture (mic + system audio) isn't wired yet
- windows graphics capture (wgc) backend
- timeline preview in frontend

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod capture;

use std::sync::Mutex;

use capture::ZoomKeyframe;

#[cfg(target_os = "macos")]
mod platform {
    use std::{process::Child, sync::Mutex};

    use screencapturekit::stream::SCStream;

    use crate::capture::{self, ZoomKeyframe};

    pub struct RecorderState(pub Mutex<Option<(SCStream, Child)>>);

    pub fn start(
        state: &RecorderState,
        output_path: String,
        width: u32,
        height: u32,
        fps: Option<u32>,
        keyframes: Vec<ZoomKeyframe>,
    ) -> Result<(), String> {
        let mut guard = state
            .0
            .lock()
            .map_err(|_| "recorder state mutex poisoned".to_string())?;
        if guard.is_some() {
            return Err("recording already in progress".into());
        }

        let session = capture::start_capture_pipeline(
            &output_path,
            width,
            height,
            fps.unwrap_or(60),
            &keyframes,
        )?;
        *guard = Some(session);
        Ok(())
    }

    pub fn stop(state: &RecorderState) -> Result<(), String> {
        let (stream, mut child) = {
            let mut guard = state
                .0
                .lock()
                .map_err(|_| "recorder state mutex poisoned".to_string())?;
            guard
                .take()
                .ok_or_else(|| "no active recording".to_string())?
        };

        stream
            .stop_capture()
            .map_err(|e| format!("failed to stop capture: {e:?}"))?;

        // drop stream before waiting so the output handler releases SyncSender and ffmpeg gets EOF to flush the mp4 moov atom
        drop(stream);

        let status = child.wait().map_err(|e| format!("ffmpeg wait failed: {e}"))?;
        if !status.success() {
            return Err(format!("ffmpeg exited with {status}"));
        }
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use std::sync::Mutex;

    use crate::capture::ZoomKeyframe;

    pub struct RecorderState(pub Mutex<Option<()>>);

    pub fn start(
        _state: &RecorderState,
        _output_path: String,
        _width: u32,
        _height: u32,
        _fps: Option<u32>,
        _keyframes: Vec<ZoomKeyframe>,
    ) -> Result<(), String> {
        Err("capture backend only implemented for macOS right now (WGC todo)".into())
    }

    pub fn stop(_state: &RecorderState) -> Result<(), String> {
        Err("capture backend only implemented for macOS right now (WGC todo)".into())
    }
}

use platform::RecorderState;

#[tauri::command]
fn start_recording(
    state: tauri::State<'_, RecorderState>,
    output_path: String,
    width: u32,
    height: u32,
    fps: Option<u32>,
    keyframes: Vec<ZoomKeyframe>,
) -> Result<(), String> {
    platform::start(&state, output_path, width, height, fps, keyframes)
}

#[tauri::command]
fn stop_recording(state: tauri::State<'_, RecorderState>) -> Result<(), String> {
    platform::stop(&state)
}

fn main() {
    tauri::Builder::default()
        .manage(RecorderState(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![start_recording, stop_recording])
        .run(tauri::generate_context!())
        .expect("error while running zoomcap");
}

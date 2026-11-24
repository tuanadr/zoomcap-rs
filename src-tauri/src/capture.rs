use serde::{Deserialize, Serialize};

// TODO: add Windows Graphics Capture (WGC) backend behind #[cfg(target_os = "windows")]

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoomKeyframe {
    pub timestamp_sec: f64,
    pub target_x: f64,
    pub target_y: f64,
    pub scale: f64,
}

pub fn cubic_ease_in_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

fn crop_rect(width: u32, height: u32, kf: &ZoomKeyframe) -> (f64, f64, f64, f64) {
    let fw = width as f64;
    let fh = height as f64;
    let scale = kf.scale.max(1.0);
    let cw = (fw / scale).clamp(2.0, fw);
    let ch = (fh / scale).clamp(2.0, fh);
    let cx = (kf.target_x - cw / 2.0).clamp(0.0, fw - cw);
    let cy = (kf.target_y - ch / 2.0).clamp(0.0, fh - ch);
    (cw, ch, cx, cy)
}

fn build_channel_expr(samples: &[(f64, f64)]) -> String {
    if samples.iter().all(|&(_, v)| (v - samples[0].1).abs() < 1e-3) {
        return format!("{:.2}", samples[0].1);
    }

    // slice transitions into 4 piecewise-linear steps sampled along cubic_ease_in_out
    // so ffmpeg's expression parser doesn't choke on deeply nested pow() trees
    const EASE_STEPS: usize = 4;

    let mut clauses: Vec<(String, String)> = Vec::new();
    if samples[0].0 > 0.0 {
        clauses.push((
            format!("lt(t,{:.3})", samples[0].0),
            format!("{:.2}", samples[0].1),
        ));
    }

    for pair in samples.windows(2) {
        let (t0, v0) = pair[0];
        let (t1, v1) = pair[1];
        let dt = t1 - t0;
        if dt <= 1e-6 {
            continue;
        }

        if (v1 - v0).abs() < 1e-3 {
            clauses.push((format!("between(t,{t0:.3},{t1:.3})"), format!("{v0:.2}")));
            continue;
        }

        for step in 0..EASE_STEPS {
            let u0 = step as f64 / EASE_STEPS as f64;
            let u1 = (step + 1) as f64 / EASE_STEPS as f64;
            let ta = t0 + dt * u0;
            let tb = t0 + dt * u1;
            let va = v0 + (v1 - v0) * cubic_ease_in_out(u0);
            let vb = v0 + (v1 - v0) * cubic_ease_in_out(u1);
            let dv = vb - va;
            let sub_dt = tb - ta;

            let val_expr = if dv.abs() < 1e-3 {
                format!("{va:.2}")
            } else {
                format!("{va:.2}+({dv:.2})*(t-{ta:.3})/{sub_dt:.3}")
            };
            clauses.push((format!("between(t,{ta:.3},{tb:.3})"), val_expr));
        }
    }

    let mut expr = format!("{:.2}", samples.last().unwrap().1);
    for (cond, val) in clauses.into_iter().rev() {
        expr = format!("if({cond},{val},{expr})");
    }
    expr
}

pub fn build_crop_filter(width: u32, height: u32, keyframes: &[ZoomKeyframe]) -> String {
    if keyframes.is_empty() {
        return format!("crop=w={width}:h={height}:x=0:y=0,scale={width}:{height}:flags=lanczos");
    }

    let mut kfs = keyframes.to_vec();
    kfs.sort_by(|a, b| a.timestamp_sec.total_cmp(&b.timestamp_sec));

    // ease in from full frame if the caller only passed a zoomed click keyframe
    if kfs[0].timestamp_sec > 0.0 && kfs[0].scale > 1.0 {
        kfs.insert(
            0,
            ZoomKeyframe {
                timestamp_sec: (kfs[0].timestamp_sec - 0.6).max(0.0),
                target_x: width as f64 / 2.0,
                target_y: height as f64 / 2.0,
                scale: 1.0,
            },
        );
    }

    let rects: Vec<(f64, (f64, f64, f64, f64))> = kfs
        .iter()
        .map(|kf| (kf.timestamp_sec.max(0.0), crop_rect(width, height, kf)))
        .collect();

    let ws: Vec<(f64, f64)> = rects.iter().map(|&(t, r)| (t, r.0)).collect();
    let hs: Vec<(f64, f64)> = rects.iter().map(|&(t, r)| (t, r.1)).collect();
    let xs: Vec<(f64, f64)> = rects.iter().map(|&(t, r)| (t, r.2)).collect();
    let ys: Vec<(f64, f64)> = rects.iter().map(|&(t, r)| (t, r.3)).collect();

    format!(
        "crop=w='{}':h='{}':x='{}':y='{}',scale={width}:{height}:flags=lanczos",
        build_channel_expr(&ws),
        build_channel_expr(&hs),
        build_channel_expr(&xs),
        build_channel_expr(&ys),
    )
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        io::{BufWriter, Write},
        process::{Child, Command, Stdio},
        sync::mpsc::{self, SyncSender, TrySendError},
        thread,
    };

    use screencapturekit::{
        output::{CMSampleBuffer, LockTrait},
        shareable_content::SCShareableContent,
        stream::{
            configuration::{pixel_format::PixelFormat, SCStreamConfiguration},
            content_filter::SCContentFilter,
            output_trait::SCStreamOutputTrait,
            output_type::SCStreamOutputType,
            SCStream,
        },
    };

    use super::{build_crop_filter, ZoomKeyframe};

    pub struct FramePipeHandler {
        tx: SyncSender<Vec<u8>>,
        width: usize,
        height: usize,
    }

    impl SCStreamOutputTrait for FramePipeHandler {
        fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
            if !matches!(of_type, SCStreamOutputType::Screen) {
                return;
            }

            // LockTrait::lock() acquires kCVPixelBufferLock_ReadOnly on the CVImageBuffer-backed pixel buffer
            let Ok(pixel_buf) = sample.get_pixel_buffer() else {
                return;
            };
            let Ok(guard) = pixel_buf.lock() else {
                return;
            };

            let stride = pixel_buf.get_bytes_per_row() as usize;
            let row_bytes = self.width * 4;
            let src = guard.as_slice();
            let rows = self.height.min(pixel_buf.get_height() as usize);

            // CoreVideo pads row strides (often 64-byte aligned), so strip padding before piping rawvideo
            let frame = if stride == row_bytes && src.len() >= row_bytes * self.height {
                src[..row_bytes * self.height].to_vec()
            } else {
                let mut buf = vec![0u8; row_bytes * self.height];
                for y in 0..rows {
                    let src_off = y * stride;
                    let dst_off = y * row_bytes;
                    if src_off + row_bytes <= src.len() {
                        buf[dst_off..dst_off + row_bytes]
                            .copy_from_slice(&src[src_off..src_off + row_bytes]);
                    }
                }
                buf
            };

            if let Err(TrySendError::Full(_)) = self.tx.try_send(frame) {
                // eprintln!("dropped frame: ffmpeg stdin channel full ({}x{})", self.width, self.height);
            }
        }
    }

    pub fn start_capture_pipeline(
        output_path: &str,
        width: u32,
        height: u32,
        fps: u32,
        keyframes: &[ZoomKeyframe],
    ) -> Result<(SCStream, Child), String> {
        // FIXME: frame timestamps drift slightly when cpu spikes >80%, need pts sync
        let vf = build_crop_filter(width, height, keyframes);
        let size = format!("{width}x{height}");
        let fps_str = fps.to_string();

        let mut child = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "bgra",
                "-s",
                &size,
                "-r",
                &fps_str,
                "-i",
                "pipe:0",
                "-vf",
                &vf,
                "-c:v",
                "h264_videotoolbox",
                "-b:v",
                "12M",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart",
                output_path,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("failed to spawn ffmpeg: {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "ffmpeg stdin was not piped".to_string())?;
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(8);
        let frame_bytes = (width as usize) * (height as usize) * 4;

        thread::spawn(move || {
            let mut writer = BufWriter::with_capacity(frame_bytes, stdin);
            while let Ok(frame) = rx.recv() {
                if writer.write_all(&frame).is_err() {
                    break;
                }
            }
            let _ = writer.flush();
        });

        let content =
            SCShareableContent::get().map_err(|e| format!("shareable content error: {e:?}"))?;
        let displays = content.displays();
        let display = displays
            .first()
            .ok_or_else(|| "no displays available to capture".to_string())?;

        let filter = SCContentFilter::new().with_display_excluding_windows(display, &[]);
        let config = SCStreamConfiguration::new()
            .set_width(width)
            .and_then(|c| c.set_height(height))
            .and_then(|c| c.set_pixel_format(PixelFormat::BGRA))
            .and_then(|c| c.set_shows_cursor(true))
            .and_then(|c| c.set_queue_depth(8))
            .map_err(|e| format!("stream config error: {e:?}"))?;

        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(
            FramePipeHandler {
                tx,
                width: width as usize,
                height: height as usize,
            },
            SCStreamOutputType::Screen,
        );
        stream
            .start_capture()
            .map_err(|e| format!("failed to start SCStream: {e:?}"))?;

        Ok((stream, child))
    }
}

#[cfg(target_os = "macos")]
pub use macos::start_capture_pipeline;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubic_easing_endpoints_and_symmetry() {
        assert!((cubic_ease_in_out(-0.5) - 0.0).abs() < 1e-9);
        assert!((cubic_ease_in_out(0.0) - 0.0).abs() < 1e-9);
        assert!((cubic_ease_in_out(0.5) - 0.5).abs() < 1e-9);
        assert!((cubic_ease_in_out(1.0) - 1.0).abs() < 1e-9);
        assert!((cubic_ease_in_out(1.5) - 1.0).abs() < 1e-9);
        assert!((cubic_ease_in_out(0.25) + cubic_ease_in_out(0.75) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn build_crop_filter_empty_keyframes() {
        let vf = build_crop_filter(1920, 1080, &[]);
        assert_eq!(vf, "crop=w=1920:h=1080:x=0:y=0,scale=1920:1080:flags=lanczos");
    }

    #[test]
    fn build_crop_filter_single_click_adds_lead_in() {
        let vf = build_crop_filter(
            1920,
            1080,
            &[ZoomKeyframe {
                timestamp_sec: 2.0,
                target_x: 960.0,
                target_y: 540.0,
                scale: 2.0,
            }],
        );
        assert!(vf.contains("between(t,1.400,"));
        assert!(vf.contains("960.00"));
        assert!(vf.contains("540.00"));
        assert!(vf.ends_with(",scale=1920:1080:flags=lanczos"));
    }

    #[test]
    fn build_crop_filter_clamps_corner_and_holds() {
        let kfs = vec![
            ZoomKeyframe {
                timestamp_sec: 1.0,
                target_x: 960.0,
                target_y: 540.0,
                scale: 1.0,
            },
            ZoomKeyframe {
                timestamp_sec: 2.0,
                target_x: 1900.0,
                target_y: 50.0,
                scale: 2.0,
            },
            ZoomKeyframe {
                timestamp_sec: 4.0,
                target_x: 1900.0,
                target_y: 50.0,
                scale: 2.0,
            },
        ];

        let vf = build_crop_filter(1920, 1080, &kfs);
        assert!(vf.contains("between(t,2.000,4.000)"));
        assert!(vf.contains("scale=1920:1080:flags=lanczos"));
        // target_x=1900 with 960px crop width must clamp x to 1920 - 960 = 960.00
        assert!(vf.contains("x='if(lt(t,1.000),0.00"));
        assert!(vf.contains("between(t,2.000,4.000),960.00,960.00)"));
        // target_y=50 with 540px crop height stays clamped at y=0.00 across all keyframes
        assert!(vf.contains("y='0.00'"));
    }
}

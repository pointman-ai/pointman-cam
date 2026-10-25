//! The extension its node runs: `cam.list`, `cam.still`, `cam.clip` and the `live` pane.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use pointman_extension::protocol::output_request::Role;
use pointman_extension::protocol::PaneOpenResponse;
use pointman_extension::v1::PaneInput;
use pointman_extension::{Cancelled, Extension, Handle, Job, Pane, Refused};
use serde_json::{json, Value};

use crate::devices::{self, Device, Found};
use crate::live::{self, Live, Settings};
use crate::{ffmpeg, gphoto, jpeg, proc, sound};

pub fn serve() -> Result<()> {
    let mut ext = Extension::new()?;
    ext.command("cam.list", list);
    ext.command("cam.still", still);
    ext.command("cam.clip", clip);
    ext.pane("live", open);
    let ran = ext.run();
    live::close_all();
    ran
}

/// This machine's settings: the default camera, and what a stream should be.
fn settings(h: &Handle) -> (String, Settings) {
    let s = h.settings();
    let number = |key: &str, default: u32| s[key].as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(default);
    let device = s["device"].as_str().unwrap_or_default().to_string();
    (
        device,
        Settings { fps: number("fps", 15).max(1), width: number("width", 1280).max(16), quality: number("quality", 5) },
    )
}

fn machine(h: &Handle) -> String {
    h.machine()
}

/// A camera named in params (by `device`, a string or a number), else the setting, else the first.
fn camera(h: &Handle, given: &Value) -> Result<(Device, Found)> {
    let (default, _) = settings(h);
    let found = devices::list();
    let wanted = match given {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    match devices::pick(&found.devices, wanted.as_deref(), &default, &machine(h)) {
        Ok(device) => Ok((device, found)),
        Err(e) if found.notes.is_empty() => Err(e),
        Err(e) => Err(anyhow!("{e:#}. {}", found.notes.join(" "))),
    }
}

/// Whether a still or clip must pause a live pane on the camera first: gphoto2's and v4l2's are one
/// program's at a time (avfoundation's are shared).
fn exclusive(device: &Device) -> bool {
    !device.is_test() && (device.is_gphoto() || cfg!(target_os = "linux"))
}

/// A camera's name as a file name.
fn file_name(device: &Device, ext: &str) -> String {
    let name: String = device.name.chars().map(|c| if matches!(c, '/' | ':' | '\\') { '-' } else { c }).collect();
    format!("{}.{ext}", name.trim())
}

// ------------------------------------------------------------------ commands

fn list(job: &Job) -> Result<Value> {
    let h = job.handle();
    let (default, _) = settings(h);
    let found = devices::list();
    let picked = devices::pick(&found.devices, None, &default, &machine(h)).ok();
    let mut text = format!("Cameras on {}:\n", machine(h));
    for d in &found.devices {
        let mut marks = Vec::new();
        if d.is_virtual() {
            marks.push("virtual");
        }
        if picked.as_ref().is_some_and(|p| p.id == d.id) {
            marks.push("the default");
        }
        let marks = if marks.is_empty() { String::new() } else { format!(" ({})", marks.join(", ")) };
        text += &format!("- **{}** {}{marks}\n", d.id, d.name);
    }
    if !found.mics.is_empty() {
        text += "\nMicrophones (the audio setting takes one):\n";
        for m in &found.mics {
            text += &format!("- **{}** {}\n", m.id, m.name);
        }
    }
    for note in &found.notes {
        text += &format!("\n{note}\n");
    }
    let listed: Vec<Value> =
        found.devices.iter().map(|d| json!({"id": d.id, "name": d.name, "virtual": d.is_virtual()})).collect();
    let mics: Vec<Value> = found.mics.iter().map(|m| json!({"id": m.id, "name": m.name})).collect();
    Ok(json!({"devices": listed, "microphones": mics, "default": picked.map(|d| d.id), "notes": found.notes,
              "text": text.trim_end()}))
}

fn still(job: &Job) -> Result<Value> {
    let h = job.handle();
    let (device, _) = camera(h, &job.params["device"])?;
    let (_, s) = settings(h);
    let asked = job.params["width"].as_u64().and_then(|w| u32::try_from(w).ok());
    let cancelled = || job.cancelled();
    let _hold = exclusive(&device).then(|| live::hold(&device));
    job.progress(None, "taking a still");
    let out = job.work.join(file_name(&device, "jpg"));
    let (width, height) = if device.is_gphoto() {
        // a real photo, as big as the camera makes it, unless a width was asked for
        let photo = gphoto::still(&device, &job.work.join("gphoto"), &cancelled)?;
        match asked {
            Some(width) => shrink(&photo, width, s.quality, &out, &cancelled)?,
            None => {
                std::fs::rename(&photo, &out)?;
                size_of(&out)?
            }
        }
    } else {
        let width = asked.unwrap_or(s.width);
        let mode = devices::mode_for(&device, width, s.fps)?;
        ffmpeg::still(&device, &mode, width, s.quality, &out, &cancelled)?
    };
    let bytes = std::fs::metadata(&out)?.len();
    let meta = json!({"camera": device.name, "device": device.id, "width": width, "height": height});
    job.output(&out, "image", Role::Main, meta);
    Ok(json!({"device": device.id, "name": device.name, "width": width, "height": height, "bytes": bytes,
              "text": format!("A still from {}, {width}×{height}", device.name)}))
}

/// A photo made at most `width` across, as a JPEG.
fn shrink(photo: &Path, width: u32, quality: u32, out: &Path, cancelled: &dyn Fn() -> bool) -> Result<(u16, u16)> {
    let mut cmd = ffmpeg::command();
    cmd.args(["-v", "error", "-i"]).arg(photo);
    cmd.args(["-vf", &ffmpeg::filters(width, None), "-frames:v", "1", "-q:v", &quality.to_string(), "-y"]).arg(out);
    let ran = proc::run(&mut cmd, Duration::from_secs(60), cancelled)?;
    if !ran.status.success() {
        bail!("couldn't make the photo {width} across: {}", ran.why());
    }
    size_of(out)
}

/// A JPEG file's size, from its header.
fn size_of(path: &Path) -> Result<(u16, u16)> {
    let mut head = Vec::new();
    std::fs::File::open(path)?.take(1 << 20).read_to_end(&mut head)?;
    jpeg::size(&head).ok_or_else(|| anyhow!("{} isn't a JPEG", path.display()))
}

fn clip(job: &Job) -> Result<Value> {
    let h = job.handle();
    let (device, _) = camera(h, &job.params["device"])?;
    let (_, s) = settings(h);
    let seconds = match &job.params["seconds"] {
        Value::Null => 10,
        v => match v.as_u64().filter(|n| (1..=60).contains(n)) {
            Some(n) => n as u32,
            None => bail!("seconds is 1 to 60, not {v}"),
        },
    };
    let _hold = exclusive(&device).then(|| live::hold(&device));
    job.progress(0.0, "starting the camera");
    let out = job.work.join(file_name(&device, "mp4"));
    let encode = ffmpeg::clip_output(s.width, s.fps, seconds, &out)?;
    let mut source = None;
    let mut cmd = ffmpeg::command();
    if device.is_gphoto() {
        // gphoto2's live view as MJPEG, timed as it comes, into ffmpeg
        gphoto::free_camera();
        let mut g = proc::spawn(&mut gphoto::live(&device, Some(seconds + 2)))?;
        let mjpeg = g.stdout.take().ok_or_else(|| anyhow!("gphoto2 gave no output"))?;
        cmd.stdin(Stdio::from(mjpeg));
        cmd.args(["-f", "mjpeg", "-use_wallclock_as_timestamps", "1", "-i", "pipe:0"]);
        source = Some(g);
    } else {
        let mode = devices::mode_for(&device, s.width, s.fps)?;
        cmd.stdin(Stdio::null());
        cmd.args(devices::input_args(&device, &mode, true));
    }
    cmd.args(&encode);
    let recorded = record(cmd, seconds, s.fps, job);
    if let Some(mut g) = source {
        proc::stop(&mut g);
    }
    let log = match recorded {
        Ok(log) => log,
        Err(e) => {
            let _ = std::fs::remove_file(&out);
            return Err(e);
        }
    };
    let (width, height) = ffmpeg::clip_info(&log).unwrap_or((0, 0));
    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    if bytes == 0 {
        bail!("{} gave no clip: {}", device.name, ffmpeg::explain(&log));
    }
    let meta = json!({"camera": device.name, "device": device.id, "width": width, "height": height, "fps": s.fps});
    job.output(&out, "video", Role::Main, meta);
    Ok(json!({"device": device.id, "name": device.name, "seconds": seconds, "width": width, "height": height,
              "fps": s.fps, "bytes": bytes,
              "text": format!("{seconds} s from {}, {width}×{height} at {} fps", device.name, s.fps)}))
}

/// ffmpeg recording a clip, its progress reported as it goes; cancelling stops it. What it said.
fn record(mut cmd: Command, seconds: u32, fps: u32, job: &Job) -> Result<String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = proc::spawn(&mut cmd)?;
    let (tx, rx) = mpsc::channel();
    if let Some(out) = child.stdout.take() {
        thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(at) = ffmpeg::progress_seconds(&line, fps) {
                    let _ = tx.send(at);
                }
            }
        });
    }
    let log = child.stderr.take().map(|mut err| {
        thread::spawn(move || {
            let mut text = String::new();
            let _ = err.read_to_string(&mut text);
            text
        })
    });
    let deadline = Instant::now() + Duration::from_secs(u64::from(seconds) + 60);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if job.cancelled() {
            proc::stop(&mut child);
            return Err(Cancelled.into());
        }
        if Instant::now() >= deadline {
            proc::stop(&mut child);
            bail!("the clip didn't finish in {} s", u64::from(seconds) + 60);
        }
        if let Some(at) = rx.try_iter().reduce(f64::max) {
            job.progress((at / f64::from(seconds)).clamp(0.0, 1.0), "recording");
        }
        thread::sleep(Duration::from_millis(100));
    };
    let log = log.and_then(|t| t.join().ok()).unwrap_or_default();
    if let Some(at) = rx.try_iter().reduce(f64::max) {
        job.progress((at / f64::from(seconds)).clamp(0.0, 1.0), "recording");
    }
    if !status.success() {
        bail!("the clip failed: {}", ffmpeg::explain(&log));
    }
    Ok(log)
}

// ------------------------------------------------------------------ the live pane

fn open(pane: &Pane) -> Result<PaneOpenResponse> {
    let h = pane.handle();
    let (device, found) = camera(h, &pane.target()["device"])?;
    let (_, s) = settings(h);
    let audio = h.settings()["audio"].as_str().unwrap_or("camera").to_string();
    let sound = sound::pick(&device, &audio, &found.mics, &machine(h))?;
    let (live, answer) = Live::open(pane.clone(), device, sound, s)?;
    let watching = live.clone();
    pane.on_viewers(move |_: &Pane, attached: u64| {
        if let Some(live) = watching.upgrade() {
            live.viewers(attached);
        }
    });
    let hinted = live.clone();
    pane.on_input(move |_: &Pane, input: &PaneInput| -> Result<()> {
        if input.kind != "hint" {
            return Err(Refused::new("the camera pane takes hints only").into());
        }
        // a hint says only what changed (the speaker's alone has no rate); a rate may be a fraction
        let event = input.json();
        let number =
            |key: &str| event[key].as_f64().filter(|n| *n >= 0.0).map(|n| n.ceil().min(f64::from(u32::MAX)) as u32);
        if let Some(live) = hinted.upgrade() {
            live.hint(number("max_fps"), number("max_width"), event["audio"].as_bool());
        }
        Ok(())
    });
    pane.on_close(move |_: &Pane, _: &str| {
        if let Some(live) = live.upgrade() {
            live.close();
        }
    });
    Ok(answer)
}

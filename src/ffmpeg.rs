//! ffmpeg, which reads the cameras (avfoundation on macOS, v4l2 on Linux, and the test card) and makes
//! stills, clips and the live pane's MJPEG.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{bail, Result};

use crate::devices::{self, Chosen, Device};
use crate::proc;

/// ffmpeg with what cam always wants: no banner, no keyboard.
pub fn command() -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-nostdin"]);
    cmd
}

/// What ffmpeg says to `args`, whatever it exits with: listing devices and modes "fails" on purpose.
pub fn output(args: &[&str]) -> Result<String> {
    let ran = proc::run(command().args(args), Duration::from_secs(15), &|| false)?;
    Ok(format!("{}{}", String::from_utf8_lossy(&ran.stdout), ran.stderr))
}

/// The filters that take a camera's frames to `width` across (never wider than it is, and even) at
/// `fps` (none: every frame it sends).
pub fn filters(width: u32, fps: Option<u32>) -> String {
    let scale = format!("scale=w=trunc(min({width}\\,iw)/2)*2:h=-2");
    match fps {
        Some(fps) => format!("fps={fps},{scale}"),
        None => scale,
    }
}

/// How long a camera is given to settle (its exposure and white balance) before a still: about a
/// second of its frames. The test card is ready at once.
fn settle_frames(device: &Device, mode: &Chosen) -> u32 {
    if device.is_test() {
        0
    } else {
        mode.fps.unwrap_or(30.0).round().clamp(1.0, 60.0) as u32
    }
}

/// One JPEG from `device`, at most `width` across, into `out`. Its size, read from the file.
pub fn still(
    device: &Device,
    mode: &Chosen,
    width: u32,
    quality: u32,
    out: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<(u16, u16)> {
    let mut cmd = command();
    cmd.args(["-v", "error"]).args(devices::input_args(device, mode, false));
    let skip = settle_frames(device, mode);
    let vf = if skip > 0 { format!("select=gte(n\\,{skip}),{}", filters(width, None)) } else { filters(width, None) };
    cmd.args(["-an", "-vf", &vf, "-frames:v", "1", "-q:v", &quality.to_string(), "-f", "image2", "-update", "1", "-y"]);
    cmd.arg(out);
    let ran = proc::run(&mut cmd, Duration::from_secs(30), cancelled)?;
    let jpeg = std::fs::read(out).unwrap_or_default();
    match crate::jpeg::size(&jpeg) {
        Some(size) if ran.status.success() => Ok(size),
        _ => bail!("{} gave no still: {}", device.name, explain(&ran.stderr)),
    }
}

/// What H.264 encoder this ffmpeg has: libx264, else (on a Mac) VideoToolbox's.
pub fn h264() -> Result<&'static [&'static str]> {
    static FOUND: OnceLock<Option<&'static [&'static str]>> = OnceLock::new();
    let found = FOUND.get_or_init(|| {
        let list = output(&["-encoders"]).unwrap_or_default();
        if list.contains("libx264") {
            Some(&["-c:v", "libx264", "-preset", "veryfast", "-crf", "23"][..])
        } else if list.contains("h264_videotoolbox") {
            Some(&["-c:v", "h264_videotoolbox", "-b:v", "6M"][..])
        } else {
            None
        }
    });
    match found {
        Some(args) => Ok(args),
        None => bail!("this machine's ffmpeg has no H.264 encoder (libx264): install a full ffmpeg"),
    }
}

/// The encoding half of a clip: H.264 an app plays anywhere, without sound, its index at the front.
/// `-progress` says how far it is on stdout.
pub fn clip_output(width: u32, fps: u32, seconds: u32, out: &Path) -> Result<Vec<String>> {
    let mut args: Vec<String> =
        ["-an", "-vf", &filters(width, Some(fps)), "-t", &seconds.to_string()].iter().map(|s| s.to_string()).collect();
    args.extend(h264()?.iter().map(|s| s.to_string()));
    for a in ["-pix_fmt", "yuv420p", "-movflags", "+faststart", "-progress", "pipe:1", "-nostats", "-y"] {
        args.push(a.into());
    }
    args.push(out.to_string_lossy().into_owned());
    Ok(args)
}

/// The size and rate of a clip's video, from what ffmpeg said of its output.
pub fn clip_info(log: &str) -> Option<(u32, u32)> {
    let output = log.split("Output #0").nth(1)?;
    let line = regex::Regex::new(r"Video: h264.*?, (\d{2,5})x(\d{2,5})").expect("a regex");
    let c = line.captures(output)?;
    Some((c[1].parse().ok()?, c[2].parse().ok()?))
}

/// How far a clip at `fps` is, in seconds, from one of `-progress`'s lines: `out_time_us=…`, or the
/// frames encoded (`frame=…`), since x264 holds frames back and says N/A for its time until it's well
/// on.
pub fn progress_seconds(line: &str, fps: u32) -> Option<f64> {
    if let Some(frames) = line.strip_prefix("frame=") {
        let frames: f64 = frames.trim().parse().ok()?;
        return (frames > 0.0).then_some(frames / f64::from(fps.max(1)));
    }
    let us: f64 = line.strip_prefix("out_time_us=")?.trim().parse().ok()?;
    (us >= 0.0).then_some(us / 1e6)
}

/// The live pane's ffmpeg: `device` as MJPEG on stdout, at most `width` across at `fps`.
pub fn live(device: &Device, mode: &Chosen, width: u32, fps: u32, quality: u32) -> Command {
    let mut cmd = command();
    cmd.args(["-v", "error"]).args(devices::input_args(device, mode, true));
    cmd.args(["-an", "-vf", &filters(width, Some(fps)), "-c:v", "mjpeg", "-q:v", &quality.to_string()]);
    cmd.args(["-f", "mjpeg", "pipe:1"]);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

/// ffmpeg's error, with what to do about the usual ones.
pub fn explain(log: &str) -> String {
    let why = proc::tail(log, 3);
    if log.contains("Device or resource busy") {
        format!("the camera is busy: another program has it ({why})")
    } else if log.contains("Failed to create AV capture input device") || log.contains("not authorized") {
        format!(
            "macOS didn't let this process use the camera: allow it under System Settings, Privacy & Security, \
             Camera ({why})"
        )
    } else {
        why
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_never_widen_and_keep_it_even() {
        assert_eq!(filters(1280, Some(15)), "fps=15,scale=w=trunc(min(1280\\,iw)/2)*2:h=-2");
        assert_eq!(filters(640, None), "scale=w=trunc(min(640\\,iw)/2)*2:h=-2");
    }

    #[test]
    fn a_clips_size_and_progress_from_what_ffmpeg_says() {
        let log = "Input #0, lavfi, from 'testsrc2':\n  Stream #0:0: Video: wrapped_avframe, yuv420p, 1280x720 [SAR 1:1 DAR 16:9], 15 fps\n\
                   Output #0, mp4, to 'out.mp4':\n  Stream #0:0: Video: h264 (avc1 / 0x31637661), yuv420p(progressive), 640x360 [SAR 1:1 DAR 16:9], q=2-31, 15 fps\n";
        assert_eq!(clip_info(log), Some((640, 360)));
        assert_eq!(clip_info("Output #0, mp4:\n"), None);
        assert_eq!(progress_seconds("out_time_us=2500000", 15), Some(2.5));
        assert_eq!(progress_seconds("out_time_us=N/A", 15), None);
        assert_eq!(progress_seconds("frame=30", 15), Some(2.0));
        assert_eq!(progress_seconds("frame=0", 15), None);
        assert_eq!(progress_seconds("fps=16.97", 15), None);
    }

    #[test]
    fn the_usual_errors_say_what_to_do() {
        assert!(
            explain("[video4linux2] ioctl(VIDIOC_STREAMON): Device or resource busy").starts_with("the camera is busy")
        );
        assert!(
            explain("Failed to create AV capture input device: Cannot use FaceTime HD").contains("Privacy & Security")
        );
        assert_eq!(explain("a\n\nb\nc\nd\n"), "b / c / d");
        assert_eq!(explain(""), "it said nothing");
    }
}

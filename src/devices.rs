//! The cameras a machine can see: ffmpeg's (avfoundation's video devices on macOS, not its screens;
//! v4l2's capture nodes on Linux), gphoto2's (cameras in PC Remote mode on USB), and `test`, a test
//! card that works anywhere. A camera is named by its number or any part of its name, and opened in a
//! size and rate it says it has, so nothing needs setting up on it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{bail, Result};
use regex::Regex;

use crate::{ffmpeg, gphoto};

/// The test card's id: ffmpeg's testsrc2, with its running time drawn on.
pub const TEST: &str = "test";
/// Where v4l2's devices are listed, if not /sys/class/video4linux: for tests.
pub const SYSFS_ENV: &str = "CAM_SYSFS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// What it's named by: avfoundation's index, `/dev/video<id>`'s number, `g<n>` for gphoto2's, or
    /// `test`.
    pub id: String,
    pub name: String,
    pub via: Via,
}

/// What reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Via {
    Ffmpeg,
    /// gphoto2, at its port (`usb:020,007`).
    Gphoto {
        port: String,
    },
}

impl Device {
    pub fn test() -> Self {
        Device { id: TEST.into(), name: "Test card".into(), via: Via::Ffmpeg }
    }

    /// One ffmpeg reads.
    pub fn ffmpeg(id: impl Into<String>, name: impl Into<String>) -> Self {
        Device { id: id.into(), name: name.into(), via: Via::Ffmpeg }
    }

    pub fn is_gphoto(&self) -> bool {
        matches!(self.via, Via::Gphoto { .. })
    }

    pub fn is_test(&self) -> bool {
        self.id == TEST
    }

    /// Whether its name says it's a virtual camera (OBS's, Imaging Edge Webcam's, Snap Camera…), which
    /// may show a placeholder or another app's picture rather than a lens.
    pub fn is_virtual(&self) -> bool {
        let words: Vec<String> = self
            .name
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(String::from)
            .collect();
        VIRTUAL.iter().any(|marker| {
            let marker: Vec<&str> = marker.split(' ').collect();
            words.windows(marker.len()).any(|w| w.iter().zip(&marker).all(|(a, b)| a == b))
        })
    }

    /// How likely it is to be the camera someone means when they name none: a body in PC Remote mode,
    /// then a camera, then a Desk View (a crop of another), then a virtual one.
    fn rank(&self) -> u8 {
        if self.is_gphoto() {
            0
        } else if self.is_virtual() {
            3
        } else if self.name.to_lowercase().contains("desk view") {
            2
        } else {
            1
        }
    }
}

/// Words in a virtual camera's name, whole words in lower case (v4l2loopback's is "Dummy video device").
const VIRTUAL: [&str; 11] =
    ["virtual", "imaging edge", "obs", "snap camera", "camo", "mmhmm", "ndi", "xsplit", "manycam", "dummy", "loopback"];

/// How ffmpeg reads cameras on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Avfoundation,
    V4l2,
}

impl Backend {
    pub fn here() -> Option<Backend> {
        if cfg!(target_os = "macos") {
            Some(Backend::Avfoundation)
        } else if cfg!(target_os = "linux") {
            Some(Backend::V4l2)
        } else {
            None
        }
    }
}

/// The cameras found, and what's missing to find more.
#[derive(Debug, Clone, Default)]
pub struct Found {
    pub devices: Vec<Device>,
    /// ffmpeg or gphoto2 isn't installed: how to get it.
    pub notes: Vec<String>,
}

/// Every camera here, ffmpeg's then gphoto2's, then the test card. A tool that isn't here leaves its
/// cameras out, and a note says how to get it.
pub fn list() -> Found {
    let mut found = Found::default();
    match Backend::here() {
        Some(Backend::Avfoundation) => {
            match ffmpeg::output(&["-f", "avfoundation", "-list_devices", "true", "-i", ""]) {
                Ok(out) => found.devices.extend(avfoundation_devices(&out)),
                Err(e) => found.notes.push(format!("{e:#}")),
            }
        }
        Some(Backend::V4l2) => {
            let sysfs = std::env::var_os(SYSFS_ENV).unwrap_or_else(|| "/sys/class/video4linux".into());
            found.devices.extend(v4l2_devices(Path::new(&sysfs)))
        }
        None => {}
    }
    match gphoto::list() {
        Ok(devices) => found.devices.extend(devices),
        Err(e) => found.notes.push(format!("For cameras in PC Remote mode (Sony, Canon, Nikon, Fuji): {e:#}")),
    }
    found.devices.push(Device::test());
    found
}

/// avfoundation's list: its video devices, without the screens (`Capture screen 0`), which aren't
/// cameras and need another permission.
pub fn avfoundation_devices(log: &str) -> Vec<Device> {
    let line = Regex::new(r"\] \[(\d+)\] (.+)$").expect("a regex");
    let mut devices = Vec::new();
    let mut video = false;
    for l in log.lines() {
        if l.contains("AVFoundation video devices") {
            video = true;
        } else if l.contains("AVFoundation audio devices") {
            video = false;
        } else if let Some(c) = line.captures(l.trim_end()).filter(|_| video) {
            let name = c[2].trim().to_string();
            if !name.starts_with("Capture screen") {
                devices.push(Device::ffmpeg(&c[1], name));
            }
        }
    }
    devices
}

/// v4l2's capture nodes, from sysfs: `/dev/videoN` with its name, by number. A camera's metadata node
/// (index 1 or more) is left out.
pub fn v4l2_devices(sysfs: &Path) -> Vec<Device> {
    let Ok(entries) = std::fs::read_dir(sysfs) else { return Vec::new() };
    let mut devices: Vec<(u32, Device)> = entries
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let n: u32 = file.strip_prefix("video")?.parse().ok()?;
            let read = |f: &str| std::fs::read_to_string(e.path().join(f)).ok().map(|s| s.trim().to_string());
            if read("index").is_some_and(|i| i != "0") {
                return None;
            }
            let name = read("name").filter(|n| !n.is_empty()).unwrap_or_else(|| file.clone());
            Some((n, Device::ffmpeg(n.to_string(), name)))
        })
        .collect();
    devices.sort_by_key(|(n, _)| *n);
    devices.into_iter().map(|(_, d)| d).collect()
}

/// The camera `wanted` names: its id (`0`, `test`, `/dev/video2`), its name, or any part of its name,
/// any case (`sony`), the likeliest of those that match. Nothing named: `default` (the setting), else
/// the likeliest camera: a body in PC Remote mode, then the first that isn't a Desk View or virtual,
/// and a virtual one only when there's nothing else.
pub fn pick(devices: &[Device], wanted: Option<&str>, default: &str, machine: &str) -> Result<Device> {
    let wanted = wanted.map(str::trim).filter(|w| !w.is_empty());
    let asked = wanted.or(Some(default.trim()).filter(|d| !d.is_empty()));
    let cameras: Vec<&Device> = devices.iter().filter(|d| !d.is_test()).collect();
    // of several, the likeliest (the first, among equals)
    let likeliest = |of: Vec<&Device>| of.into_iter().min_by_key(|d| d.rank()).cloned();
    let Some(asked) = asked else {
        return match likeliest(cameras) {
            Some(camera) => Ok(camera),
            None => bail!("{machine} has no camera cam can see (device test is a test card)"),
        };
    };
    let key = asked.strip_prefix("/dev/video").unwrap_or(asked).to_lowercase();
    let found = devices
        .iter()
        .find(|d| d.id.to_lowercase() == key)
        .or_else(|| devices.iter().find(|d| d.name.to_lowercase() == key))
        .cloned()
        .or_else(|| likeliest(devices.iter().filter(|d| d.name.to_lowercase().contains(&key)).collect()));
    match found {
        Some(d) => Ok(d),
        None => {
            let have: Vec<String> = cameras.iter().map(|d| format!("{} {}", d.id, d.name)).collect();
            let have = if have.is_empty() { "none".to_string() } else { have.join(", ") };
            bail!("no camera '{asked}' on {machine}: it has {have} (and test, a test card)")
        }
    }
}

// ------------------------------------------------------------------ modes

/// A size a camera has, and the frame rates it gives at that size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub min_fps: f64,
    pub max_fps: f64,
}

/// What the camera is asked for, and what comes out: the size it captures, the rate asked of it,
/// and (on Linux) its pixel format.
#[derive(Debug, Clone, PartialEq)]
pub struct Chosen {
    pub width: u32,
    pub height: u32,
    pub fps: Option<f64>,
    pub input_format: Option<String>,
}

/// avfoundation's modes, from what it says when it's asked for one it hasn't: `1280x720@[1.000000
/// 30.000000]fps`, one line each.
pub fn avfoundation_modes(log: &str) -> Vec<Mode> {
    let line = Regex::new(r"(\d+)x(\d+)@\[([\d.]+)\s+([\d.]+)\]fps").expect("a regex");
    let mut modes: Vec<Mode> = Vec::new();
    for c in line.captures_iter(log) {
        let mode = Mode {
            width: c[1].parse().unwrap_or(0),
            height: c[2].parse().unwrap_or(0),
            min_fps: c[3].parse().unwrap_or(0.0),
            max_fps: c[4].parse().unwrap_or(0.0),
        };
        if mode.width > 0 && mode.height > 0 && mode.max_fps > 0.0 && !modes.contains(&mode) {
            modes.push(mode);
        }
    }
    modes
}

/// The mode to capture in for `width` across at `fps`: one with that rate (or the fastest there is),
/// and of those the narrowest at least `width` across (or the widest there is). avfoundation takes
/// only a rate that's the top of one of the mode's ranges, so that's what's asked for.
pub fn choose(modes: &[Mode], width: u32, fps: u32) -> Option<Chosen> {
    let fps = f64::from(fps);
    let fast_enough: Vec<&Mode> = modes.iter().filter(|m| m.max_fps + 0.01 >= fps).collect();
    let pool: Vec<&Mode> = if fast_enough.is_empty() {
        let top = modes.iter().map(|m| m.max_fps).fold(0.0, f64::max);
        modes.iter().filter(|m| m.max_fps + 0.01 >= top).collect()
    } else {
        fast_enough
    };
    let wide_enough = pool.iter().filter(|m| m.width >= width).min_by_key(|m| (m.width, m.height));
    let size = wide_enough.or_else(|| pool.iter().max_by_key(|m| (m.width, m.height)))?;
    // at that size: the slowest range that's fast enough, else the fastest
    let rates: Vec<f64> =
        pool.iter().filter(|m| m.width == size.width && m.height == size.height).map(|m| m.max_fps).collect();
    let rate = rates
        .iter()
        .copied()
        .filter(|r| r + 0.01 >= fps)
        .reduce(f64::min)
        .or_else(|| rates.iter().copied().reduce(f64::max))?;
    Some(Chosen { width: size.width, height: size.height, fps: Some(rate), input_format: None })
}

/// v4l2's formats (`-list_formats all`): each pixel format with its sizes, `Compressed: mjpeg :
/// Motion-JPEG : 640x480 1280x720`.
/// One of a v4l2 camera's pixel formats, and the sizes it has in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Format {
    pub compressed: bool,
    /// ffmpeg's name for it: `mjpeg`, `yuyv422`.
    pub name: String,
    pub sizes: Vec<(u32, u32)>,
}

pub fn v4l2_formats(log: &str) -> Vec<Format> {
    let line = Regex::new(r"(Raw|Compressed)\s*:\s*(\S+)\s*:[^:]*:\s*(.*)$").expect("a regex");
    let size = Regex::new(r"^(\d+)x(\d+)$").expect("a regex");
    log.lines()
        .filter_map(|l| {
            let c = line.captures(l.trim_end())?;
            let sizes = c[3]
                .split_whitespace()
                .filter_map(|s| {
                    let m = size.captures(s)?;
                    Some((m[1].parse().ok()?, m[2].parse().ok()?))
                })
                .collect();
            Some(Format { compressed: &c[1] == "Compressed", name: c[2].to_string(), sizes })
        })
        .collect()
}

/// The v4l2 format and size for `width` across: MJPEG where the camera has it (a webcam sends raw
/// frames at a fraction of the rate), the narrowest size at least that wide, or the widest. A camera
/// that lists no sizes (a stepwise one) is left to choose its own.
pub fn choose_v4l2(formats: &[Format], width: u32, fps: u32) -> Option<Chosen> {
    // each format's size for that width, then the format whose size is: the narrowest wide enough, else
    // the widest
    let best = |mjpeg_only: bool| {
        formats
            .iter()
            .filter(|f| !mjpeg_only || (f.compressed && f.name == "mjpeg"))
            .filter_map(|f| {
                let size = f.sizes.iter().filter(|(w, _)| *w >= width).min().or_else(|| f.sizes.iter().max())?;
                Some((f.name.clone(), *size))
            })
            .min_by_key(|(_, (w, h))| if *w >= width { (0, *w, *h) } else { (1, u32::MAX - w, 0) })
    };
    let (format, (w, h)) = best(true).or_else(|| best(false))?;
    Some(Chosen { width: w, height: h, fps: Some(f64::from(fps)), input_format: Some(format) })
}

/// What each camera was found to have, by name, so it's asked once per run of the extension.
static MODES: Mutex<Option<HashMap<String, Vec<Mode>>>> = Mutex::new(None);
static FORMATS: Mutex<Option<HashMap<String, Vec<Format>>>> = Mutex::new(None);

/// Forget what a camera has (it failed to open in a mode it had: it may be another camera now).
pub fn forget(device: &Device) {
    if let Some(m) = MODES.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.remove(&device.name);
    }
    if let Some(f) = FORMATS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        f.remove(&device.name);
    }
}

/// How to ask this camera for about `width` across at `fps`: its mode, asked of it the first time.
/// The test card is any size.
pub fn mode_for(device: &Device, width: u32, fps: u32) -> Result<Chosen> {
    if device.is_test() {
        let height = (width * 9 / 16).max(2) & !1;
        return Ok(Chosen { width, height, fps: Some(f64::from(fps)), input_format: None });
    }
    match Backend::here() {
        Some(Backend::Avfoundation) => {
            let mut cache = MODES.lock().unwrap_or_else(|e| e.into_inner());
            let modes = cache.get_or_insert_with(HashMap::new);
            if !modes.contains_key(&device.name) {
                // asking for a size no camera has makes avfoundation list the ones it does, without
                // starting it (its light stays off)
                let mut args = vec!["-f", "avfoundation", "-video_size", "1x1", "-i"];
                let input = avfoundation_input(device);
                args.push(&input);
                let found = avfoundation_modes(&ffmpeg::output(&args)?);
                if found.is_empty() {
                    bail!("{} didn't say what sizes it has", device.name);
                }
                modes.insert(device.name.clone(), found);
            }
            let found = &modes[&device.name];
            choose(found, width, fps).ok_or_else(|| anyhow::anyhow!("{} has no mode to use", device.name))
        }
        Some(Backend::V4l2) => {
            let mut cache = FORMATS.lock().unwrap_or_else(|e| e.into_inner());
            let formats = cache.get_or_insert_with(HashMap::new);
            if !formats.contains_key(&device.name) {
                let path = format!("/dev/video{}", device.id);
                let out = ffmpeg::output(&["-f", "v4l2", "-list_formats", "all", "-i", &path])?;
                formats.insert(device.name.clone(), v4l2_formats(&out));
            }
            Ok(choose_v4l2(&formats[&device.name], width, fps).unwrap_or(Chosen {
                width: 0,
                height: 0,
                fps: Some(f64::from(fps)),
                input_format: None,
            }))
        }
        None => bail!("cameras are read on macOS and Linux only"),
    }
}

/// avfoundation's input for a camera: its name, which stays the same when others come and go, unless
/// the name has a colon (avfoundation's video:audio separator), then its number.
pub fn avfoundation_input(device: &Device) -> String {
    if device.name.contains(':') {
        device.id.clone()
    } else {
        device.name.clone()
    }
}

/// ffmpeg's arguments that read `device` in `mode`: everything up to and with its `-i`. `live` reads
/// the test card at its own rate, as a camera would come.
pub fn input_args(device: &Device, mode: &Chosen, live: bool) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut push = |a: &str| args.push(a.to_string());
    if device.is_test() {
        if live {
            push("-re");
        }
        push("-f");
        push("lavfi");
        push("-i");
        let rate = mode.fps.unwrap_or(15.0);
        push(&format!("testsrc2=size={}x{}:rate={}", mode.width, mode.height, fmt_rate(rate)));
        return args;
    }
    match Backend::here() {
        Some(Backend::Avfoundation) => {
            push("-f");
            push("avfoundation");
            if mode.width > 0 {
                push("-video_size");
                push(&format!("{}x{}", mode.width, mode.height));
            }
            if let Some(rate) = mode.fps {
                push("-framerate");
                push(&fmt_rate(rate));
            }
            push("-i");
            push(&avfoundation_input(device));
        }
        _ => {
            push("-f");
            push("v4l2");
            if let Some(format) = &mode.input_format {
                push("-input_format");
                push(format);
            }
            if mode.width > 0 {
                push("-video_size");
                push(&format!("{}x{}", mode.width, mode.height));
            }
            if let Some(rate) = mode.fps {
                push("-framerate");
                push(&fmt_rate(rate));
            }
            push("-i");
            push(&format!("/dev/video{}", device.id));
        }
    }
    args
}

/// A rate as ffmpeg takes it: `30`, or `29.97` with what it needs.
fn fmt_rate(rate: f64) -> String {
    if (rate - rate.round()).abs() < 0.001 {
        format!("{}", rate.round() as i64)
    } else {
        format!("{rate:.6}").trim_end_matches('0').to_string()
    }
}

/// What comes out for `width` across from a camera capturing in `mode`: never wider than it is, and
/// an even height.
pub fn scaled(mode: &Chosen, width: u32) -> (u32, u32) {
    if mode.width == 0 || mode.width <= width {
        return (mode.width, mode.height);
    }
    let height = (u64::from(mode.height) * u64::from(width) / u64::from(mode.width)) as u32;
    (width, (height.max(2) + 1) & !1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "\
[AVFoundation indev @ 0x1] AVFoundation video devices:
[AVFoundation indev @ 0x1] [0] MacBook Pro Camera
[AVFoundation indev @ 0x1] [1] iPhone (2) Camera
[AVFoundation indev @ 0x1] [2] Sony a6400: USB
[AVFoundation indev @ 0x1] [3] Capture screen 0
[AVFoundation indev @ 0x1] AVFoundation audio devices:
[AVFoundation indev @ 0x1] [0] MacBook Pro Microphone
[in#0 @ 0x2] Error opening input: Input/output error
";

    #[test]
    fn avfoundations_cameras_without_its_screens_or_microphones() {
        let devices = avfoundation_devices(LIST);
        let names: Vec<(&str, &str)> = devices.iter().map(|d| (d.id.as_str(), d.name.as_str())).collect();
        assert_eq!(names, [("0", "MacBook Pro Camera"), ("1", "iPhone (2) Camera"), ("2", "Sony a6400: USB")]);
        // by name, unless the name has avfoundation's colon in it
        assert_eq!(avfoundation_input(&devices[0]), "MacBook Pro Camera");
        assert_eq!(avfoundation_input(&devices[2]), "2");
    }

    #[test]
    fn a_camera_by_number_name_or_any_part_of_it() {
        let mut devices = avfoundation_devices(LIST);
        devices.push(Device::test());
        let pick = |w: Option<&str>, d: &str| pick(&devices, w, d, "mac-m4").map(|d| d.id);
        assert_eq!(pick(None, "").unwrap(), "0");
        assert_eq!(pick(Some(""), "sony").unwrap(), "2"); // nothing named: the setting
        assert_eq!(pick(Some("1"), "sony").unwrap(), "1");
        assert_eq!(pick(Some("SONY"), "").unwrap(), "2");
        assert_eq!(pick(Some("iphone (2) camera"), "").unwrap(), "1");
        assert_eq!(pick(Some("camera"), "").unwrap(), "0"); // several: the first
        assert_eq!(pick(Some("test"), "").unwrap(), "test");
        assert_eq!(pick(Some("/dev/video1"), "").unwrap(), "1");
        let e = pick(Some("canon"), "").unwrap_err().to_string();
        assert_eq!(
            e,
            "no camera 'canon' on mac-m4: it has 0 MacBook Pro Camera, 1 iPhone (2) Camera, 2 Sony a6400: USB \
             (and test, a test card)"
        );
        let e = super::pick(&[Device::test()], None, "", "pi").unwrap_err().to_string();
        assert_eq!(e, "pi has no camera cam can see (device test is a test card)");
    }

    /// mac-m4's list on 16 October, with Imaging Edge Webcam and OBS installed.
    const MAC_M4: &str = "\
[AVFoundation indev @ 0x1] AVFoundation video devices:
[AVFoundation indev @ 0x1] [0] Sony Camera (Imaging Edge)
[AVFoundation indev @ 0x1] [1] MacBook Pro Camera
[AVFoundation indev @ 0x1] [2] OBS Virtual Camera
[AVFoundation indev @ 0x1] [3] iPhone (2) Camera
[AVFoundation indev @ 0x1] [4] MacBook Pro Desk View Camera
[AVFoundation indev @ 0x1] [5] iPhone (2) Desk View Camera
[AVFoundation indev @ 0x1] [6] Capture screen 0
[AVFoundation indev @ 0x1] AVFoundation audio devices:
";

    #[test]
    fn with_none_named_the_likeliest_real_camera() {
        let mac = avfoundation_devices(MAC_M4);
        let virtuals: Vec<&str> = mac.iter().filter(|d| d.is_virtual()).map(|d| d.name.as_str()).collect();
        assert_eq!(virtuals, ["Sony Camera (Imaging Edge)", "OBS Virtual Camera"]);
        let pick = |devices: &[Device], wanted: Option<&str>| pick(devices, wanted, "", "mac-m4").unwrap().name;
        assert_eq!(pick(&mac, None), "MacBook Pro Camera"); // not Imaging Edge's, first in the list
                                                            // a body in PC Remote mode comes first, and wins a name both match
        let sony = Device {
            id: "g0".into(),
            name: "Sony Alpha-A6400 (PC Control)".into(),
            via: Via::Gphoto { port: "usb:020,007".into() },
        };
        let with_sony: Vec<Device> = mac.iter().cloned().chain([sony.clone()]).collect();
        assert_eq!(pick(&with_sony, None), sony.name);
        assert_eq!(pick(&with_sony, Some("sony")), sony.name);
        assert_eq!(pick(&mac, Some("sony")), "Sony Camera (Imaging Edge)"); // the only one named so
        assert_eq!(pick(&mac, Some("0")), "Sony Camera (Imaging Edge)"); // by number, as asked
                                                                         // Desk View after a camera, a virtual one only when there's nothing else
        let rest = |names: &[&str]| -> Vec<Device> {
            names.iter().enumerate().map(|(i, n)| Device::ffmpeg(i.to_string(), *n)).collect()
        };
        assert_eq!(pick(&rest(&["MacBook Pro Desk View Camera", "iPhone (2) Camera"]), None), "iPhone (2) Camera");
        assert_eq!(
            pick(&rest(&["OBS Virtual Camera", "MacBook Pro Desk View Camera"]), None),
            "MacBook Pro Desk View Camera"
        );
        assert_eq!(
            pick(&rest(&["Sony Camera (Imaging Edge)", "OBS Virtual Camera"]), None),
            "Sony Camera (Imaging Edge)"
        );
        // whole words: Jacob's camera isn't OBS's
        let names =
            ["Jacob's Camera", "Camouflage Cam", "Snap Camera", "Dummy video device (0x0000)", "NDI Video", "mmhmm"];
        let flags: Vec<bool> = rest(&names).iter().map(Device::is_virtual).collect();
        assert_eq!(flags, [false, false, true, true, true, true]);
    }

    #[test]
    fn v4l2s_capture_nodes_from_sysfs() {
        let dir = tempfile::tempdir().unwrap();
        for (node, name, index) in
            [("video0", "HD Webcam", "0"), ("video1", "HD Webcam", "1"), ("video10", "Cam Link 4K", "0")]
        {
            std::fs::create_dir_all(dir.path().join(node)).unwrap();
            std::fs::write(dir.path().join(node).join("name"), format!("{name}\n")).unwrap();
            std::fs::write(dir.path().join(node).join("index"), index).unwrap();
        }
        std::fs::create_dir_all(dir.path().join("v4l-subdev0")).unwrap();
        let devices = v4l2_devices(dir.path());
        let names: Vec<(&str, &str)> = devices.iter().map(|d| (d.id.as_str(), d.name.as_str())).collect();
        assert_eq!(names, [("0", "HD Webcam"), ("10", "Cam Link 4K")]);
        assert!(v4l2_devices(&dir.path().join("none")).is_empty());
    }

    const MODES: &str = "\
[avfoundation @ 0x1] Selected video size (1x1) is not supported by the device.
[avfoundation @ 0x1] Supported modes:
[avfoundation @ 0x1]   640x480@[1.000000 30.000000]fps
[avfoundation @ 0x1]   1280x720@[1.000000 30.000000]fps
[avfoundation @ 0x1]   1280x720@[60.000000 60.000000]fps
[avfoundation @ 0x1]   1920x1080@[1.000000 30.000000]fps
[avfoundation @ 0x1]   1920x1080@[1.000000 30.000000]fps
[avfoundation @ 0x1]   3840x2160@[1.000000 15.000000]fps
";

    #[test]
    fn a_mode_the_camera_has_at_the_top_of_a_range() {
        let modes = avfoundation_modes(MODES);
        assert_eq!(modes.len(), 5); // the same twice is once
        let chosen = |w, f| choose(&modes, w, f).map(|c| (c.width, c.height, c.fps.unwrap()));
        assert_eq!(chosen(1280, 15), Some((1280, 720, 30.0)));
        assert_eq!(chosen(1000, 15), Some((1280, 720, 30.0)));
        assert_eq!(chosen(640, 30), Some((640, 480, 30.0)));
        assert_eq!(chosen(1280, 50), Some((1280, 720, 60.0)));
        assert_eq!(chosen(3840, 15), Some((3840, 2160, 15.0)));
        assert_eq!(chosen(3840, 30), Some((1920, 1080, 30.0))); // 4K is too slow: the widest that isn't
        assert_eq!(chosen(5000, 120), Some((1280, 720, 60.0))); // none fast enough: the fastest
        assert_eq!(choose(&[], 1280, 15), None);
    }

    const FORMATS: &str = "\
[video4linux2,v4l2 @ 0x1] Raw       :     yuyv422 :           YUYV 4:2:2 : 640x480 160x120 1280x720
[video4linux2,v4l2 @ 0x1] Compressed:       mjpeg :          Motion-JPEG : 640x480 1280x720 1920x1080
";

    #[test]
    fn v4l2_takes_mjpeg_where_it_can() {
        let formats = v4l2_formats(FORMATS);
        assert_eq!(formats.len(), 2);
        assert_eq!(
            formats[0],
            Format { compressed: false, name: "yuyv422".into(), sizes: vec![(640, 480), (160, 120), (1280, 720)] }
        );
        let chosen = |w| choose_v4l2(&formats, w, 15).map(|c| (c.input_format.unwrap(), c.width, c.height));
        assert_eq!(chosen(1280), Some(("mjpeg".into(), 1280, 720)));
        assert_eq!(chosen(1000), Some(("mjpeg".into(), 1280, 720)));
        assert_eq!(chosen(4000), Some(("mjpeg".into(), 1920, 1080)));
        let raw = v4l2_formats(FORMATS.lines().next().unwrap());
        assert_eq!(choose_v4l2(&raw, 800, 15).map(|c| (c.width, c.input_format)), Some((1280, Some("yuyv422".into()))));
        assert_eq!(choose_v4l2(&[], 800, 15), None);
    }

    #[test]
    fn the_test_card_and_scaling() {
        let card = mode_for(&Device::test(), 1280, 15).unwrap();
        assert_eq!((card.width, card.height), (1280, 720));
        assert_eq!(
            input_args(&Device::test(), &card, true),
            ["-re", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=15"]
        );
        let mode = Chosen { width: 1920, height: 1080, fps: Some(30.0), input_format: None };
        assert_eq!(scaled(&mode, 1280), (1280, 720));
        assert_eq!(scaled(&mode, 4000), (1920, 1080));
        assert_eq!(scaled(&Chosen { width: 640, height: 480, ..mode.clone() }, 500), (500, 376));
        assert_eq!(fmt_rate(30.0), "30");
        assert_eq!(fmt_rate(29.97), "29.97");
    }
}

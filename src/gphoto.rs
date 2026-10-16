//! gphoto2, the second way in: cameras on USB in their PC Remote mode (most Sony, Canon, Nikon and
//! Fuji bodies), with no app from their maker. Live view comes as MJPEG (`--capture-movie --stdout`),
//! and a still is a real photo, taken by the camera and downloaded at its full size.
//!
//! On macOS, `PTPCamera` (`ptpcamerad`) takes every PTP camera that's plugged in, so it's stopped just
//! before gphoto2 needs one; launchd starts it again later.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Result};
use regex::Regex;

use crate::devices::{Device, Via};
use crate::proc;

pub fn command() -> Command {
    Command::new("gphoto2")
}

/// gphoto2 for one camera: its model, at its port.
fn camera(device: &Device) -> Command {
    let mut cmd = command();
    if let Via::Gphoto { port } = &device.via {
        cmd.args(["--port", port]);
    }
    cmd.args(["--camera", &device.name]);
    cmd
}

/// The cameras gphoto2 can see that can take a picture or show live view (a phone in PTP mode can
/// do neither), numbered `g0`, `g1`… in its order. An error says gphoto2 isn't here.
pub fn list() -> Result<Vec<Device>> {
    let ran = proc::run(command().arg("--auto-detect"), Duration::from_secs(15), &|| false)?;
    let mut devices = Vec::new();
    for (model, port) in detected(&String::from_utf8_lossy(&ran.stdout)) {
        let device = Device { id: format!("g{}", devices.len()), name: model, via: Via::Gphoto { port } };
        if captures(&device) {
            devices.push(device);
        }
    }
    Ok(devices)
}

/// `--auto-detect`'s table: each camera's model and port, without disks (a card reader).
pub fn detected(table: &str) -> Vec<(String, String)> {
    let row = Regex::new(r"^(.+?)\s{2,}((?:usb|ptpip|serial|ip):\S*)\s*$").expect("a regex");
    table
        .lines()
        .filter_map(|l| row.captures(l.trim_end()))
        .map(|c| (c[1].trim().to_string(), c[2].to_string()))
        .collect()
}

/// Whether `--abilities` says it takes pictures or shows live view, asked once per model. One whose
/// abilities can't be read is kept: trying it says more than leaving it out.
fn captures(device: &Device) -> bool {
    static KNOWN: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    let known = known.get_or_insert_with(HashMap::new);
    if let Some(yes) = known.get(&device.name) {
        return *yes;
    }
    let Ok(ran) = proc::run(camera(device).arg("--abilities"), Duration::from_secs(15), &|| false) else {
        return true;
    };
    let yes = can_capture(&String::from_utf8_lossy(&ran.stdout)).unwrap_or(true);
    known.insert(device.name.clone(), yes);
    yes
}

/// From `--abilities`: the items under "Capture choices" include Image or Preview. None if it didn't
/// list them.
pub fn can_capture(abilities: &str) -> Option<bool> {
    let mut lines = abilities.lines().skip_while(|l| !l.trim_start().starts_with("Capture choices"));
    let first = lines.next()?;
    let mut choices: Vec<String> = Vec::new();
    if let Some((_, said)) = first.split_once(':') {
        choices.push(said.trim().to_string());
    }
    for l in lines {
        match l.split_once(':') {
            Some((label, said)) if label.trim().is_empty() => choices.push(said.trim().to_string()),
            _ => break,
        }
    }
    Some(choices.iter().any(|c| c == "Image" || c == "Preview"))
}

/// Stop what holds a PTP camera on macOS, so gphoto2 can claim it.
pub fn free_camera() {
    if cfg!(target_os = "macos") {
        let _ = proc::run(Command::new("killall").args(["PTPCamera", "ptpcamerad"]), Duration::from_secs(5), &|| false);
    }
}

/// Whether gphoto2 failed because something else had the camera.
fn claimed(log: &str) -> bool {
    log.contains("Could not claim the USB device") || log.contains("Could not claim the device")
}

/// gphoto2's error, with what to do about the usual ones.
pub fn explain(log: &str) -> String {
    let why = proc::tail(log, 3);
    if claimed(log) {
        if cfg!(target_os = "macos") {
            format!("another program has the camera, even with PTPCamera stopped ({why})")
        } else {
            format!("another program has the camera (on GNOME, gvfs's gphoto2 volume monitor; stop it) ({why})")
        }
    } else if log.contains("No camera found") || log.contains("Could not detect any camera") {
        format!("the camera isn't there: plugged in, on, and in PC Remote mode? ({why})")
    } else {
        why
    }
}

/// A photo taken by the camera and downloaded into `dir`: the JPEG (a RAW beside it is left there).
pub fn still(device: &Device, dir: &Path, cancelled: &dyn Fn() -> bool) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let pattern = dir.join("photo.%C");
    let take = || {
        let mut cmd = camera(device);
        cmd.args(["--capture-image-and-download", "--force-overwrite", "--filename"]).arg(&pattern);
        proc::run(&mut cmd, Duration::from_secs(60), cancelled)
    };
    free_camera();
    let mut ran = take()?;
    if !ran.status.success() && claimed(&ran.stderr) {
        free_camera();
        ran = take()?;
    }
    let jpeg = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_stem().is_some_and(|s| s == "photo"))
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg")));
    match jpeg {
        Some(path) if ran.status.success() => Ok(path),
        None if ran.status.success() => {
            bail!("{} saved no JPEG (it's set to RAW only?): set it to JPEG, or RAW+JPEG", device.name)
        }
        _ => bail!("{} took no photo: {}", device.name, explain(&ran.stderr)),
    }
}

/// gphoto2 sending the camera's live view to stdout as MJPEG: for `seconds`, or until stopped.
pub fn live(device: &Device, seconds: Option<u32>) -> Command {
    let mut cmd = camera(device);
    match seconds {
        Some(s) => cmd.arg(format!("--capture-movie={s}s")),
        None => cmd.arg("--capture-movie"),
    };
    cmd.arg("--stdout");
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

/// The size a camera's live view came in last, by model, for the next open to say before its first
/// frame; until one has come, 3:2 at 1024 across, as most bodies send.
static LIVE_SIZE: Mutex<Option<HashMap<String, (u16, u16)>>> = Mutex::new(None);

pub fn live_size(device: &Device) -> (u16, u16) {
    let sizes = LIVE_SIZE.lock().unwrap_or_else(|e| e.into_inner());
    sizes.as_ref().and_then(|s| s.get(&device.name).copied()).unwrap_or((1024, 680))
}

pub fn saw_size(device: &Device, size: (u16, u16)) {
    let mut sizes = LIVE_SIZE.lock().unwrap_or_else(|e| e.into_inner());
    sizes.get_or_insert_with(HashMap::new).insert(device.name.clone(), size);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_detects_table_without_its_disks() {
        let table = "\
Model                          Port
----------------------------------------------------------
Sony Alpha-A6400 (PC Control)  usb:020,007
Canon EOS R6                   usb:020,009
Apple iPhone (PTP mode)        usb:020,011
Mass Storage Camera            disk:/Volumes/EOS_DIGITAL
";
        assert_eq!(
            detected(table),
            [
                ("Sony Alpha-A6400 (PC Control)".to_string(), "usb:020,007".to_string()),
                ("Canon EOS R6".into(), "usb:020,009".into()),
                ("Apple iPhone (PTP mode)".into(), "usb:020,011".into()),
            ]
        );
        assert!(detected("Model  Port\n------\n").is_empty());
    }

    #[test]
    fn abilities_say_whether_it_takes_pictures() {
        let camera = "\
Abilities for camera             : Sony Alpha-A6400 (PC Control)
Serial port support              : no
USB support                      : yes
Capture choices                  :
                                 : Image
                                 : Preview
                                 : Trigger Capture
Configuration support            : yes
";
        let phone = "\
Abilities for camera             : Apple iPhone (PTP mode)
Capture choices                  : Capture not supported by the driver
Configuration support            : no
";
        assert_eq!(can_capture(camera), Some(true));
        assert_eq!(can_capture(phone), Some(false));
        assert_eq!(can_capture("*** Error: no camera ***"), None);
    }

    #[test]
    fn the_usual_errors_say_what_to_do() {
        assert!(explain("*** Error: No camera found. ***").starts_with("the camera isn't there"));
        assert!(explain("Could not claim the USB device").starts_with("another program has the camera"));
    }
}

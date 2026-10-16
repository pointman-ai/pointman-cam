//! The live pane (Pointman's docs/dev/video.md): a camera's frames as `pane.frame`, captured only while
//! a path is attached, so its light is off otherwise. ffmpeg (or gphoto2) writes MJPEG; the splitter
//! cuts it into JPEGs; the newest waits in one slot, and a sender takes whatever is newest each time,
//! so a slow viewer never backs up the camera. A hint restarts the capture at its rate and width
//! (`max_fps: 0` stops it). If the camera stops sending, it's tried again, longer apart each time,
//! and found again by name if it came back somewhere else.
//!
//! Sound, when the pane has it, is a second ffmpeg: ADTS AAC cut into whole frames and sent in order as
//! `pane.audio` about every 40 ms, timed by its samples. It runs while a path is attached and the app's
//! speaker is on (`audio: false` stops it), so a microphone is never open with no one listening.

use std::io::Read;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use pointman_extension::Pane;
use serde_json::{json, Value};

use crate::devices::{self, Device};
use crate::jpeg::Splitter;
use crate::sound::{self, Source};
use crate::{adts, ffmpeg, gphoto, proc};

/// How much sound goes in each `pane.audio`.
const SOUND_EVERY_MS: u64 = 40;

/// What the machine's settings say a stream should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub fps: u32,
    pub width: u32,
    pub quality: u32,
}

/// The widths a hint is rounded up to, so dragging a window doesn't restart the camera at each pixel.
const WIDTHS: [u32; 8] = [320, 480, 640, 960, 1280, 1920, 2560, 3840];

/// What a capture is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Want {
    pub fps: u32,
    pub width: u32,
}

/// The stream for this many paths attached and the app's last hint: none when nobody's watching or the
/// app paused it; otherwise the settings, lowered to the hint (its width rounded up to [`WIDTHS`]).
pub fn wanted(settings: Settings, viewers: u64, max_fps: u32, max_width: u32) -> Option<Want> {
    if viewers == 0 || max_fps == 0 {
        return None;
    }
    let width = WIDTHS.iter().copied().find(|w| *w >= max_width).unwrap_or(WIDTHS[WIDTHS.len() - 1]);
    Some(Want { fps: settings.fps.min(max_fps), width: settings.width.min(width) })
}

/// One frame, as it came out of the splitter.
struct Frame {
    jpeg: Vec<u8>,
    width: u16,
    height: u16,
    captured_ms: u64,
}

/// The newest frame, and whether its capture is over.
#[derive(Default)]
struct Slot {
    frame: Mutex<Option<Frame>>,
    ready: Condvar,
    over: AtomicBool,
}

/// One run of ffmpeg or gphoto2 for the pane.
struct Capture {
    want: Want,
    child: Arc<Mutex<Child>>,
    slot: Arc<Slot>,
}

impl Capture {
    fn stop(&self) {
        self.slot.over.store(true, Ordering::SeqCst);
        self.slot.ready.notify_all();
        let child = self.child.clone();
        // gently, off this thread: gphoto2 takes up to 2 s to leave live view
        thread::spawn(move || proc::stop(&mut lock(&child)));
    }
}

/// One run of ffmpeg for the pane's sound.
struct Hearing {
    child: Arc<Mutex<Child>>,
    over: Arc<AtomicBool>,
}

impl Hearing {
    fn stop(&self) {
        self.over.store(true, Ordering::SeqCst);
        let child = self.child.clone();
        thread::spawn(move || proc::stop(&mut lock(&child)));
    }
}

/// Which of the pane's two captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pictures,
    Sound,
}
struct State {
    viewers: u64,
    max_fps: u32,
    max_width: u32,
    closed: bool,
    capture: Option<Capture>,
    /// Counts each capture started, so a capture that ended knows if it's still the pane's.
    run: u64,
    /// Captures in a row that ended on their own.
    failures: u32,
    /// The app's speaker: on until it says off.
    speaker: bool,
    hearing: Option<Hearing>,
    /// The same two for sound.
    heard: u64,
    deaf: u32,
}

/// A live pane on one camera.
pub struct Live {
    pane: Pane,
    device: Mutex<Device>,
    /// Where its sound comes from, if it has any.
    sound: Option<Source>,
    settings: Settings,
    state: Mutex<State>,
    /// A still or a clip has the camera for now (one gphoto2 or v4l2 can't share).
    held: AtomicU32,
    me: Weak<Live>,
}

/// The live panes open now: what keeps each alive until it closes (its pane's handlers hold it weakly,
/// as it holds its pane), and what a still or clip pauses.
static OPEN: Mutex<Vec<Arc<Live>>> = Mutex::new(Vec::new());

impl Live {
    /// A live pane on `device` (with sound from `sound`), and what the pane opens with: the camera's
    /// name, and the size and rate its frames will have, and its sound's. It's kept until
    /// [`close`](Self::close).
    pub fn open(pane: Pane, device: Device, sound: Option<Source>, settings: Settings) -> Result<(Weak<Live>, Value)> {
        let live = Arc::new_cyclic(|me| Live {
            pane,
            device: Mutex::new(device),
            sound,
            settings,
            state: Mutex::new(State {
                viewers: 0,
                max_fps: settings.fps,
                max_width: settings.width,
                closed: false,
                capture: None,
                run: 0,
                failures: 0,
                speaker: true,
                hearing: None,
                heard: 0,
                deaf: 0,
            }),
            held: AtomicU32::new(0),
            me: me.clone(),
        });
        let answer = live.opening()?;
        let weak = Arc::downgrade(&live);
        lock(&OPEN).push(live);
        Ok((weak, answer))
    }

    fn opening(&self) -> Result<Value> {
        let device = lock(&self.device).clone();
        let (width, height) = if device.is_gphoto() {
            let (w, h) = gphoto::live_size(&device);
            (u32::from(w), u32::from(h))
        } else {
            let mode = devices::mode_for(&device, self.settings.width, self.settings.fps)?;
            devices::scaled(&mode, self.settings.width)
        };
        let mut video = json!({"codec": "mjpeg", "width": width, "height": height, "fps": self.settings.fps});
        if self.sound.is_some() {
            video["audio"] = sound::opening();
        }
        Ok(json!({"title": device.name, "video": video}))
    }

    /// The node's count of paths attached.
    pub fn viewers(&self, attached: u64) {
        lock(&self.state).viewers = attached;
        self.apply();
    }

    /// The app's hint, which says only what changed: at most this rate (0 pauses the pictures), this
    /// width, and whether its speaker's on.
    pub fn hint(&self, max_fps: Option<u32>, max_width: Option<u32>, audio: Option<bool>) {
        {
            let mut state = lock(&self.state);
            if let Some(fps) = max_fps {
                state.max_fps = fps;
            }
            if let Some(width) = max_width {
                state.max_width = if width == 0 { self.settings.width } else { width };
            }
            if let Some(on) = audio {
                state.speaker = on;
            }
        }
        self.apply();
    }

    /// Its pane closed: stop the camera, and let it go.
    pub fn close(&self) {
        lock(&self.state).closed = true;
        self.apply();
        lock(&OPEN).retain(|l| !std::ptr::eq(Arc::as_ptr(l), self));
    }

    /// Start, stop or restart the captures to match what's wanted now.
    fn apply(&self) {
        let mut state = lock(&self.state);
        let pictures = self.apply_pictures(&mut state);
        let sound = self.apply_sound(&mut state);
        drop(state);
        if let Some(run) = pictures {
            self.again(Kind::Pictures, run, false);
        }
        if let Some(run) = sound {
            self.again(Kind::Sound, run, false);
        }
    }

    /// The pictures' part of [`apply`](Self::apply): the run that failed to start, if one did.
    fn apply_pictures(&self, state: &mut State) -> Option<u64> {
        let want = if state.closed || self.held.load(Ordering::SeqCst) > 0 {
            None
        } else {
            wanted(self.settings, state.viewers, state.max_fps, state.max_width)
        };
        if state.capture.as_ref().map(|c| c.want) == want {
            return None;
        }
        if let Some(old) = state.capture.take() {
            old.stop();
        }
        let want = want?;
        state.run += 1;
        let run = state.run;
        match self.start(want, run) {
            Ok(capture) => state.capture = Some(capture),
            Err(e) => {
                self.pane.handle().log(format!("live {}: {e:#}", lock(&self.device).name));
                return Some(run);
            }
        }
        None
    }

    /// The sound's part: on while a path is attached and the speaker's on.
    fn apply_sound(&self, state: &mut State) -> Option<u64> {
        let want = self.sound.as_ref().filter(|_| !state.closed && state.viewers > 0 && state.speaker);
        if want.is_some() == state.hearing.is_some() {
            return None;
        }
        if let Some(old) = state.hearing.take() {
            old.stop();
        }
        let source = want?;
        state.heard += 1;
        let run = state.heard;
        match self.listen(source, run) {
            Ok(hearing) => state.hearing = Some(hearing),
            Err(e) => {
                self.pane.handle().log(format!("live sound: {e:#}"));
                return Some(run);
            }
        }
        None
    }

    fn start(&self, want: Want, run: u64) -> Result<Capture> {
        let device = lock(&self.device).clone();
        let mut cmd = if device.is_gphoto() {
            gphoto::free_camera();
            gphoto::live(&device, None)
        } else {
            let mode = devices::mode_for(&device, want.width, want.fps)?;
            ffmpeg::live(&device, &mode, want.width, want.fps, self.settings.quality)
        };
        let mut child = proc::spawn(&mut cmd)?;
        let stdout = child.stdout.take();
        let log = keep_log(child.stderr.take());
        let child = Arc::new(Mutex::new(child));
        let slot = Arc::new(Slot::default());
        let (me, reading) = (self.me.clone(), slot.clone());
        let gphoto = device.is_gphoto();
        thread::spawn(move || {
            let started = Instant::now();
            let mut frames = 0u64;
            if let Some(out) = stdout {
                frames = read(out, &reading, |frame| {
                    if gphoto && frames == 0 {
                        gphoto::saw_size(&device, (frame.width, frame.height));
                    }
                });
            }
            if reading.over.load(Ordering::SeqCst) {
                return; // stopped: what's wanted changed, or the pane closed
            }
            reading.over.store(true, Ordering::SeqCst);
            reading.ready.notify_all();
            if let Some(live) = me.upgrade() {
                let why = proc::tail(&lock(&log), 3);
                let ran = started.elapsed().as_secs();
                live.pane
                    .handle()
                    .log(format!("live {}: it stopped after {frames} frames in {ran} s: {why}", device.name));
                live.again(Kind::Pictures, run, frames > 0 && ran >= 10);
            }
        });
        let (pane, sending) = (self.pane.clone(), slot.clone());
        let every = if gphoto { Some(Duration::from_secs_f64(1.0 / f64::from(want.fps.max(1)))) } else { None };
        thread::spawn(move || send(&pane, &sending, every));
        Ok(Capture { want, child, slot })
    }

    /// ffmpeg for the pane's sound, and a reader sending it, in order, as `pane.audio`.
    fn listen(&self, source: &Source, run: u64) -> Result<Hearing> {
        let mut child = proc::spawn(&mut sound::command(source))?;
        let stdout = child.stdout.take();
        let log = keep_log(child.stderr.take());
        let over = Arc::new(AtomicBool::new(false));
        let (me, pane, stopped) = (self.me.clone(), self.pane.clone(), over.clone());
        thread::spawn(move || {
            let started = Instant::now();
            let sent = stdout.map_or(0, |out| hear(out, &pane, &stopped));
            if stopped.load(Ordering::SeqCst) {
                return; // stopped: the speaker's off, no one's watching, or the pane closed
            }
            if let Some(live) = me.upgrade() {
                let (why, ran) = (proc::tail(&lock(&log), 3), started.elapsed().as_secs());
                live.pane.handle().log(format!("live sound: it stopped after {sent} sends in {ran} s: {why}"));
                live.again(Kind::Sound, run, sent > 0 && ran >= 10);
            }
        });
        Ok(Hearing { child: Arc::new(Mutex::new(child)), over })
    }

    /// A capture ended on its own (or didn't start): try again, longer apart each time it happens in a
    /// row (1 s, doubling to 30), unless it ran a good while first. Before trying pictures again, the
    /// camera is found again by its name, and asked its modes afresh.
    fn again(&self, kind: Kind, run: u64, ran_well: bool) {
        let wait = {
            let mut state = lock(&self.state);
            let state = &mut *state;
            let (current, failures) = match kind {
                Kind::Pictures => (state.run, &mut state.failures),
                Kind::Sound => (state.heard, &mut state.deaf),
            };
            if current != run {
                return;
            }
            *failures = if ran_well { 1 } else { *failures + 1 };
            let wait = Duration::from_secs(1u64 << failures.saturating_sub(1).min(5)).min(Duration::from_secs(30));
            match kind {
                Kind::Pictures => state.capture.take().map(|gone| gone.stop()),
                Kind::Sound => state.hearing.take().map(|gone| gone.stop()),
            };
            wait
        };
        let me = self.me.clone();
        thread::spawn(move || {
            thread::sleep(wait);
            let Some(live) = me.upgrade() else { return };
            let current = match kind {
                Kind::Pictures => lock(&live.state).run,
                Kind::Sound => lock(&live.state).heard,
            };
            if current != run {
                return; // something else has started it, or stopped it, since
            }
            if kind == Kind::Pictures {
                let mut device = lock(&live.device).clone();
                devices::forget(&device);
                if let Some(found) = devices::list().devices.into_iter().find(|d| d.name == device.name) {
                    device = found;
                    *lock(&live.device) = device;
                }
            }
            live.apply();
        });
    }
}

/// Pause every live pane on `device` while the guard lives (a still or a clip needs the camera to
/// itself), and start them again after.
pub fn hold(device: &Device) -> Hold {
    let lives: Vec<Arc<Live>> = lock(&OPEN).iter().filter(|l| lock(&l.device).name == device.name).cloned().collect();
    for live in &lives {
        live.held.fetch_add(1, Ordering::SeqCst);
        live.apply();
    }
    if !lives.is_empty() {
        // let the stopped capture let go of the camera
        thread::sleep(Duration::from_millis(if device.is_gphoto() { 2500 } else { 500 }));
    }
    Hold(lives)
}

pub struct Hold(Vec<Arc<Live>>);

impl Drop for Hold {
    fn drop(&mut self) {
        for live in &self.0 {
            live.held.fetch_sub(1, Ordering::SeqCst);
            live.apply();
        }
    }
}

/// JPEGs out of the tool's stdout into the slot, each newest replacing the last, until it ends or the
/// capture is stopped. How many came.
fn read(mut out: impl Read, slot: &Slot, mut each: impl FnMut(&Frame)) -> u64 {
    let mut splitter = Splitter::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut count = 0;
    while !slot.over.load(Ordering::SeqCst) {
        let n = match out.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for jpeg in splitter.push(&buf[..n]) {
            let Some((width, height)) = crate::jpeg::size(&jpeg) else { continue };
            let frame = Frame { jpeg, width, height, captured_ms: now_ms() };
            each(&frame);
            count += 1;
            *lock(&slot.frame) = Some(frame);
            slot.ready.notify_all();
        }
    }
    count
}

/// ADTS out of ffmpeg's stdout to the pane, about every [`SOUND_EVERY_MS`], in order, until it ends or
/// is stopped. How many `pane.audio`s went.
fn hear(mut out: impl Read, pane: &Pane, stopped: &AtomicBool) -> u64 {
    let (mut splitter, mut batcher) = (adts::Splitter::new(), adts::Batcher::new(SOUND_EVERY_MS));
    let mut buf = vec![0u8; 16 * 1024];
    let mut sent = 0;
    while !stopped.load(Ordering::SeqCst) {
        let n = match out.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for frame in splitter.push(&buf[..n]) {
            if let Some((aac, captured_ms)) = batcher.push(frame, now_ms()) {
                if !stopped.load(Ordering::SeqCst) && pane.audio(&aac, captured_ms) {
                    sent += 1;
                }
            }
        }
    }
    sent
}

/// The last few KiB of what a tool says, kept for when it stops on its own.
fn keep_log(stderr: Option<impl Read + Send + 'static>) -> Arc<Mutex<String>> {
    let log = Arc::new(Mutex::new(String::new()));
    if let Some(mut err) = stderr {
        let log = log.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = err.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut log = lock(&log);
                log.push_str(&String::from_utf8_lossy(&buf[..n]));
                if log.len() > 8192 {
                    let cut = log.len() - 4096;
                    let cut = (cut..log.len()).find(|i| log.is_char_boundary(*i)).unwrap_or(0);
                    log.drain(..cut);
                }
            }
        });
    }
    log
}

/// The newest frame to the pane each time, until the capture's over; at most one per `every`.
fn send(pane: &Pane, slot: &Slot, every: Option<Duration>) {
    loop {
        let frame = {
            let mut frame = lock(&slot.frame);
            loop {
                if slot.over.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(f) = frame.take() {
                    break f;
                }
                frame = slot.ready.wait_timeout(frame, Duration::from_millis(500)).unwrap_or_else(|e| e.into_inner()).0;
            }
        };
        let sent = Instant::now();
        pane.frame(&frame.jpeg, frame.width, frame.height, frame.captured_ms);
        if let Some(every) = every {
            thread::sleep(every.saturating_sub(sent.elapsed()));
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTINGS: Settings = Settings { fps: 15, width: 1280, quality: 5 };

    #[test]
    fn a_stream_only_while_watched_and_never_past_the_settings() {
        assert_eq!(wanted(SETTINGS, 0, 15, 1280), None);
        assert_eq!(wanted(SETTINGS, 1, 0, 1280), None); // paused
        assert_eq!(wanted(SETTINGS, 2, 15, 1280), Some(Want { fps: 15, width: 1280 }));
        assert_eq!(wanted(SETTINGS, 1, 30, 4000), Some(Want { fps: 15, width: 1280 }));
        assert_eq!(wanted(SETTINGS, 1, 5, 700), Some(Want { fps: 5, width: 960 }));
        assert_eq!(wanted(SETTINGS, 1, 15, 641), wanted(SETTINGS, 1, 15, 900)); // no restart for a few pixels
        assert_eq!(wanted(SETTINGS, 1, 15, 100), Some(Want { fps: 15, width: 320 }));
    }

    #[test]
    fn the_slot_keeps_only_the_newest() {
        let jpeg = |n: u8| {
            let mut j = vec![0xff, 0xd8, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x30, 0x00, 0x40, 0x01, 0x01, 0x11, 0x00];
            j.extend([0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00, n, 0xff, 0xd9]);
            j
        };
        let stream: Vec<u8> = (1..=5).flat_map(jpeg).collect();
        let slot = Slot::default();
        let mut seen = Vec::new();
        let count = read(&stream[..], &slot, |f| seen.push((f.width, f.height)));
        assert_eq!((count, seen.len(), seen[0]), (5, 5, (64, 48)));
        assert_eq!(lock(&slot.frame).as_ref().unwrap().jpeg, jpeg(5));
    }
}

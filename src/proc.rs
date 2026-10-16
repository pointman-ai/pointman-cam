//! Running the tools cam drives (ffmpeg, gphoto2): to the end with a time limit and a way to cancel,
//! and stopped gently, so a camera leaves live view as it should.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use pointman_extension::Cancelled;

/// What a run left behind.
#[derive(Debug)]
pub struct Ran {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Ran {
    /// Its log's last lines, for an error that says why.
    pub fn why(&self) -> String {
        tail(&self.stderr, 3)
    }
}

/// How to get a tool that isn't on this machine's PATH.
pub fn missing(tool: &str) -> anyhow::Error {
    match tool {
        "gphoto2" => anyhow!(
            "gphoto2 isn't installed on this machine: brew install gphoto2 on a Mac, apt install gphoto2 on \
             Debian or Ubuntu, or nix's gphoto2 package"
        ),
        _ => anyhow!(
            "{tool} isn't installed on this machine: brew install {tool} on a Mac, apt install {tool} on Debian \
             or Ubuntu, or nix's {tool} package"
        ),
    }
}

/// Starts `cmd`, or says how to get its program.
pub fn spawn(cmd: &mut Command) -> Result<Child> {
    let tool = cmd.get_program().to_string_lossy().into_owned();
    cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => missing(&tool),
        _ => anyhow!("can't start {tool}: {e}"),
    })
}

/// Runs `cmd` to its end, its output kept, for at most `limit`. `cancelled` is asked every 50 ms; once
/// it says yes, the tool is stopped and the run is [`Cancelled`].
pub fn run(cmd: &mut Command, limit: Duration, cancelled: &dyn Fn() -> bool) -> Result<Ran> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = spawn(cmd)?;
    let out = child.stdout.take().map(|mut s| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            buf
        })
    });
    let err = child.stderr.take().map(|mut s| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    });
    let deadline = Instant::now() + limit;
    let tool = cmd.get_program().to_string_lossy().into_owned();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Ok(status);
        }
        if cancelled() {
            stop(&mut child);
            break Err(anyhow::Error::from(Cancelled));
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            break Err(anyhow!("{tool} didn't finish in {} s", limit.as_secs()));
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = out.and_then(|t| t.join().ok()).unwrap_or_default();
    let stderr = err.and_then(|t| t.join().ok()).unwrap_or_default();
    match status {
        Ok(status) => Ok(Ran { status, stdout, stderr }),
        Err(e) if e.is::<Cancelled>() => Err(e),
        Err(e) => bail!("{e}: {}", tail(&stderr, 3)),
    }
}

/// Stops a tool as Ctrl-C would (ffmpeg and gphoto2 both end cleanly on it: gphoto2 takes the camera
/// out of live view), then kills it if it hasn't gone in 2 s.
pub fn stop(child: &mut Child) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: a signal to our own child, which hasn't been reaped (try_wait said it's running)
        unsafe { libc::kill(pid, libc::SIGINT) };
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The last `n` lines of a log that say something, on one line.
pub fn tail(log: &str, n: usize) -> String {
    let lines: Vec<&str> = log.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let text = lines[lines.len().saturating_sub(n)..].join(" / ");
    if text.is_empty() {
        "it said nothing".into()
    } else {
        text
    }
}

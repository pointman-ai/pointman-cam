//! AAC in ADTS out of ffmpeg's stream: whole frames, each with how many samples it holds and at what
//! rate, so sound can be sent in short runs and timed by its samples.

/// The sample rates an ADTS header's index stands for.
const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// One ADTS frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub bytes: Vec<u8>,
    pub rate: u32,
    pub samples: u32,
}

/// Bytes in, whole ADTS frames out, in order. Bytes that aren't a frame are skipped to the next sync
/// word.
#[derive(Debug, Default)]
pub struct Splitter {
    buf: Vec<u8>,
}

impl Splitter {
    pub fn new() -> Self {
        Splitter::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            let Some(at) = self.buf.windows(2).position(|w| w[0] == 0xff && w[1] & 0xf0 == 0xf0) else {
                let keep = usize::from(self.buf.last() == Some(&0xff));
                let drop = self.buf.len() - keep;
                self.buf.drain(..drop);
                return frames;
            };
            self.buf.drain(..at);
            let Some(h) = self.buf.get(..7) else { return frames };
            let header = if h[1] & 1 == 1 { 7 } else { 9 };
            let len = (usize::from(h[3] & 3) << 11) | (usize::from(h[4]) << 3) | usize::from(h[5] >> 5);
            let rate = RATES.get(usize::from((h[2] >> 2) & 0xf)).copied();
            let (Some(rate), true) = (rate, len >= header) else {
                self.buf.drain(..1); // not a frame: look for the next sync word
                continue;
            };
            if self.buf.len() < len {
                return frames;
            }
            let samples = 1024 * (u32::from(h[6] & 3) + 1);
            frames.push(Frame { bytes: self.buf.drain(..len).collect(), rate, samples });
        }
    }
}

/// Runs of whole frames to send at least `every_ms` long each, timed by their samples: the first run
/// is when it was read less its length, and each after it follows on by its samples, so the times
/// don't jitter with when the bytes arrived. If the wall's clock gets more than 250 ms ahead of it (a
/// stall: sound was lost), it goes on from the wall's. Never back: sound that comes faster than it's
/// played (ffmpeg's `-re` sends its first half second at once) keeps its own clock.
#[derive(Debug)]
pub struct Batcher {
    every_ms: u64,
    held: Vec<u8>,
    held_samples: u64,
    rate: u32,
    /// When the next run's first sample was captured, by the samples' clock.
    next_ms: Option<f64>,
}

impl Batcher {
    pub fn new(every_ms: u64) -> Self {
        Batcher { every_ms, held: Vec::new(), held_samples: 0, rate: 48000, next_ms: None }
    }

    /// A frame read at `now_ms`; a run to send, with when its first sample was captured, once there's
    /// enough.
    pub fn push(&mut self, frame: Frame, now_ms: u64) -> Option<(Vec<u8>, u64)> {
        self.rate = frame.rate;
        self.held.extend_from_slice(&frame.bytes);
        self.held_samples += u64::from(frame.samples);
        let held_ms = self.held_samples as f64 * 1000.0 / f64::from(self.rate);
        if held_ms < self.every_ms as f64 {
            return None;
        }
        let by_wall = now_ms as f64 - held_ms;
        let start = match self.next_ms {
            Some(next) if by_wall - next <= 250.0 => next,
            _ => by_wall,
        };
        self.next_ms = Some(start + held_ms);
        self.held_samples = 0;
        Some((std::mem::take(&mut self.held), start.round() as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ADTS frame of `len` bytes: AAC-LC at 48 kHz, mono, no CRC.
    fn adts(len: usize) -> Vec<u8> {
        let mut f =
            vec![0xff, 0xf1, 0x4c, 0x40 | (len >> 11) as u8, (len >> 3) as u8, ((len & 7) << 5) as u8 | 0x1f, 0xfc];
        f.resize(len, 0x11);
        f
    }

    #[test]
    fn frames_split_whole_however_the_bytes_arrive() {
        let frames = [adts(300), adts(7), adts(1200)];
        let mut stream = b"\x00\x12junk".to_vec();
        for f in &frames {
            stream.extend(f);
        }
        for chunk in [1, 3, 50, stream.len()] {
            let mut s = Splitter::new();
            let got: Vec<Frame> = stream.chunks(chunk).flat_map(|c| s.push(c)).collect();
            assert_eq!(got.iter().map(|f| f.bytes.clone()).collect::<Vec<_>>(), frames, "in chunks of {chunk}");
            assert!(got.iter().all(|f| f.rate == 48000 && f.samples == 1024));
        }
    }

    #[test]
    fn ffmpegs_own_adts_splits_whole() {
        let real = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sound.adts")).unwrap();
        let frames = Splitter::new().push(&real);
        assert!(frames.len() >= 8, "{}", frames.len());
        assert_eq!(frames.iter().map(|f| f.bytes.len()).sum::<usize>(), real.len());
    }

    #[test]
    fn runs_are_timed_by_their_samples_not_their_arrival() {
        let mut b = Batcher::new(40);
        let f = || Frame { bytes: adts(10), rate: 48000, samples: 1024 };
        assert_eq!(b.push(f(), 1000), None); // 21 ms: not enough yet
        let (run, first) = b.push(f(), 1043).unwrap();
        assert_eq!((run.len(), first), (20, 1000)); // 1043 less its 42.7 ms
                                                    // the next comes late by the wall, but on time by its samples
        b.push(f(), 1100);
        assert_eq!(b.push(f(), 1130).unwrap().1, 1043);
        // a stall of a second: from the wall again
        b.push(f(), 2500);
        assert_eq!(b.push(f(), 2530).unwrap().1, 2487);
        // a burst, faster than it plays: its own clock, never back
        let times: Vec<u64> = (0..6).filter_map(|_| b.push(f(), 2531)).map(|(_, t)| t).collect();
        assert_eq!(times, [2530, 2573, 2615]); // 2487.3 on by 42.7 ms each
    }
}

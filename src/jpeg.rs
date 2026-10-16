//! JPEGs out of ffmpeg's MJPEG stream: whole frames, split on their SOI and EOI markers, and each one's
//! size from its frame header.
//!
//! A frame is walked marker by marker rather than searched for `FF D9`: a quantisation or Huffman table
//! may hold those two bytes, but after the scan header the entropy-coded data stuffs every `FF` (`FF 00`),
//! so the first marker there that isn't a restart is the frame's end.

const SOI: u8 = 0xd8;
const EOI: u8 = 0xd9;
const SOS: u8 = 0xda;

/// Bytes in, whole JPEGs out, in order. What comes before a frame's SOI is skipped.
#[derive(Debug, Default)]
pub struct Splitter {
    buf: Vec<u8>,
}

/// Where a frame that starts at 0 ends, if all of it is here.
enum Walk {
    Whole(usize),
    More,
    /// Not a JPEG after all: drop this many bytes and look for the next SOI.
    Bad(usize),
}

impl Splitter {
    pub fn new() -> Self {
        Splitter::default()
    }

    /// Takes the next bytes; gives back each frame they finish.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            match find_soi(&self.buf) {
                Some(0) => {}
                Some(at) => {
                    self.buf.drain(..at);
                }
                None => {
                    // keep a last FF: it may be the start of the next SOI
                    let keep = usize::from(self.buf.last() == Some(&0xff));
                    let drop = self.buf.len() - keep;
                    self.buf.drain(..drop);
                    return frames;
                }
            }
            match walk(&self.buf) {
                Walk::Whole(end) => frames.push(self.buf.drain(..end).collect()),
                Walk::More => return frames,
                Walk::Bad(skip) => {
                    self.buf.drain(..skip);
                }
            }
        }
    }

    /// Bytes held for a frame not yet whole.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

fn find_soi(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == [0xff, SOI])
}

/// A frame from its SOI at 0: its segments by their lengths, then the scan's data to the next marker.
fn walk(buf: &[u8]) -> Walk {
    let mut i = 2;
    loop {
        // a marker: FF (any fill FFs), then its code
        if i >= buf.len() {
            return Walk::More;
        }
        if buf[i] != 0xff {
            return Walk::Bad(2);
        }
        while i + 1 < buf.len() && buf[i + 1] == 0xff {
            i += 1;
        }
        let Some(&code) = buf.get(i + 1) else { return Walk::More };
        match code {
            EOI => return Walk::Whole(i + 2),
            SOI | 0x00 => return Walk::Bad(i.max(2)),
            0x01 | 0xd0..=0xd7 => i += 2,
            _ => {
                let Some(len) = buf.get(i + 2..i + 4).map(|l| usize::from(u16::from_be_bytes([l[0], l[1]]))) else {
                    return Walk::More;
                };
                if len < 2 {
                    return Walk::Bad(i.max(2));
                }
                i += 2 + len;
                if code == SOS {
                    // the entropy-coded data: FF 00 is a stuffed FF and FF D0-D7 a restart; any other is
                    // the next marker
                    loop {
                        let Some(at) = buf.get(i..).and_then(|rest| rest.iter().position(|b| *b == 0xff)) else {
                            return Walk::More;
                        };
                        i += at;
                        match buf.get(i + 1) {
                            None => return Walk::More,
                            Some(0x00 | 0xd0..=0xd7 | 0xff) => i += 1,
                            Some(_) => break,
                        }
                    }
                }
            }
        }
    }
}

/// A JPEG's width and height, from its frame header (any SOF but the DHT, JPG and DAC markers that
/// share the range).
pub fn size(jpeg: &[u8]) -> Option<(u16, u16)> {
    if !jpeg.starts_with(&[0xff, SOI]) {
        return None;
    }
    let mut i = 2;
    while i + 4 <= jpeg.len() {
        if jpeg[i] != 0xff {
            return None;
        }
        let code = jpeg[i + 1];
        if code == 0xff {
            i += 1;
            continue;
        }
        let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
        if matches!(code, 0xc0..=0xcf) && !matches!(code, 0xc4 | 0xc8 | 0xcc) {
            let header = jpeg.get(i + 5..i + 9)?;
            let (height, width) =
                (u16::from_be_bytes([header[0], header[1]]), u16::from_be_bytes([header[2], header[3]]));
            return (width > 0 && height > 0).then_some((width, height));
        }
        if code == SOS || code == EOI || len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG's shape, as ffmpeg's encoder writes one: APP0, a table holding FF D9, a frame header,
    /// the scan header, and data with a stuffed FF and a restart marker.
    fn jpeg(n: u8, width: u16, height: u16) -> Vec<u8> {
        let mut j = vec![0xff, SOI];
        j.extend([0xff, 0xe0, 0x00, 0x07, b'J', b'F', b'I', b'F', 0x00]);
        j.extend([0xff, 0xdb, 0x00, 0x06, 0x00, 0xff, EOI, 0x10]); // DQT with FF D9 in its values
        let (w, h) = (width.to_be_bytes(), height.to_be_bytes());
        j.extend([0xff, 0xc0, 0x00, 0x0b, 0x08, h[0], h[1], w[0], w[1], 0x01, 0x01, 0x11, 0x00]);
        j.extend([0xff, SOS, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00]);
        j.extend([n, 0x12, 0xff, 0x00, 0x34, 0xff, 0xd3, 0x56, n]);
        j.extend([0xff, EOI]);
        j
    }

    #[test]
    fn frames_split_whole_however_the_bytes_arrive() {
        let frames = [jpeg(1, 1280, 720), jpeg(2, 1280, 720), jpeg(3, 640, 360)];
        let mut stream = b"noise".to_vec();
        for f in &frames {
            stream.extend(f);
        }
        for chunk in [1, 2, 3, 7, 64, stream.len()] {
            let mut s = Splitter::new();
            let got: Vec<Vec<u8>> = stream.chunks(chunk).flat_map(|c| s.push(c)).collect();
            assert_eq!(got, frames, "in chunks of {chunk}");
            assert_eq!(s.pending(), 0);
        }
    }

    #[test]
    fn a_frame_cut_short_is_dropped_at_the_next_soi() {
        let (a, b) = (jpeg(1, 64, 48), jpeg(2, 64, 48));
        let mut s = Splitter::new();
        // a's scan data cut off where its end would be, then b: b's SOI ends a's scan as a marker that
        // isn't its EOI, so a is dropped, not glued to b
        let mut stream = a[..a.len() - 2].to_vec();
        stream.extend(&b);
        assert_eq!(s.push(&stream), [b]);
        // a half frame waits for the rest
        assert!(s.push(&a[..10]).is_empty());
        assert_eq!(s.push(&a[10..]), [a]);
    }

    #[test]
    fn its_size_is_its_frame_headers() {
        assert_eq!(size(&jpeg(1, 1280, 720)), Some((1280, 720)));
        assert_eq!(size(&jpeg(1, 64, 48)), Some((64, 48)));
        assert_eq!(size(b"not a jpeg"), None);
        assert_eq!(size(&jpeg(1, 0, 48)), None);
    }
}

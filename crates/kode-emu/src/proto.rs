//! The wire between the firmware's kemu and the Mac (kemu_proto.h).
//!
//! A frame is `KD`, a type byte, a little-endian u32 length and the payload.
//! The decoder resyncs on `KD` and treats a length over [`MAX_LEN`] as
//! corruption, so boot noise on the UART or a split TCP read never costs
//! more than a frame.
//!
//! Pixels travel as [`FB_PACKED`] (kemu_pack16, PackBits over RGB565) and
//! leave the decoder as [`FB_RECT`], so nothing past it knows the
//! difference. The page speaks the same frames over its WebSocket.

pub const MAGIC: &[u8; 2] = b"KD";
pub const HEADER: usize = 7;
pub const MAX_LEN: usize = 600_000;

pub const HELLO: u8 = 0x01;
pub const FB_RECT: u8 = 0x10;
pub const PANEL: u8 = 0x11;
pub const FB_PACKED: u8 = 0x12;
pub const LED: u8 = 0x20;
pub const HAPTIC: u8 = 0x21;
/// rate u32, ch u8, then s16le: the speaker, kode-emu to the page (the
/// sound is esp-emu's I2S, not kemu's).
pub const PCM_OUT: u8 = 0x30;
/// rate u32, ch u8: the Mac's microphone is on; the page sends PCM_IN.
pub const MIC_START: u8 = 0x31;
/// It is off.
pub const MIC_STOP: u8 = 0x32;
pub const POWER_OFF: u8 = 0x40;
pub const TOUCH: u8 = 0x80;
pub const BUTTONS: u8 = 0x81;
/// s16le at the last MIC_START's rate: the Mac's microphone, page to
/// kode-emu, for the codec's receiver.
pub const PCM_IN: u8 = 0x82;
pub const BATTERY: u8 = 0x83;
/// Raw console input: the board's USB-Serial/JTAG RX.
pub const CONSOLE_IN: u8 = 0x84;
/// unix_us u64: the Mac's clock, for the P4's RTC.
pub const TIME: u8 = 0x88;

/// Ten i32 LE, for the board's chips rather than kemu: the acceleration in milli-g
/// (x, y, z), the rotation in milli-degrees per second, the magnetic field in
/// milligauss, and the die's temperature in tenths of a degree - in the board's
/// own axes (x right, y toward the top, z out of the glass).
pub const MOTION: u8 = 0x8D;

/// ep u8 (the address, direction bit included), then one whole IN transfer
/// the app armed: what the Kode Dot sends the computer.
pub const USB_IN: u8 = 0x60;
/// kind u8, a u8, b u16: what the device did to the bus (pull-up, stall, ...).
pub const USB_DEV: u8 = 0x61;
/// ep u8, bytes u32: OUT bytes the app took, so the host may send more.
pub const USB_ACK: u8 = 0x62;
/// kind u8: 0 cable out, 1 cable in, 2 suspend, 3 resume.
pub const USB_BUS: u8 = 0x8A;
/// The 8 bytes of a SETUP packet.
pub const USB_SETUP: u8 = 0x8B;
/// ep u8 (0-7), then one OUT chunk of up to 4096 bytes.
pub const USB_OUT: u8 = 0x8C;

/// One frame, header and all.
pub fn encode(type_: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(type_);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// kemu_pack16()'s stream back to RGB565; None if it is cut short.
///
/// A control byte below 128 is followed by that many plus one literal
/// pixels; one of 128 or more by a single pixel repeated (c - 126) times.
pub fn unpack16(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 2);
    let mut i = 0;
    while i < data.len() {
        let c = data[i] as usize;
        i += 1;
        let k = if c < 128 { (c + 1) * 2 } else { 2 };
        let piece = data.get(i..i + k)?;
        if c < 128 {
            out.extend_from_slice(piece);
        } else {
            for _ in 0..c - 126 {
                out.extend_from_slice(piece);
            }
        }
        i += k;
    }
    Some(out)
}

/// An FB_PACKED payload as the FB_RECT it stands for, or None if its pixels
/// are not the rectangle its header promises.
fn unpack_rect(payload: &[u8]) -> Option<Vec<u8>> {
    let head = payload.get(..8)?;
    let w = u16::from_le_bytes([head[4], head[5]]) as usize;
    let h = u16::from_le_bytes([head[6], head[7]]) as usize;
    let pixels = unpack16(&payload[8..])?;
    if pixels.len() != w * h * 2 {
        return None;
    }
    let mut out = head.to_vec();
    out.extend_from_slice(&pixels);
    Some(out)
}

/// HELLO's header: what every boot says first, whatever it cut short.
const HELLO_HEAD: &[u8; HEADER] = b"KD\x01\x06\x00\x00\x00";
/// A whole HELLO: its header, kemu's protocol version (any), and the
/// panel's 410 x 502 - all 13 bytes, so a payload holding the 7-byte header
/// is not taken for a boot.
const HELLO_LEN: usize = HEADER + 6;
const HELLO_GEOMETRY: [u8; 4] = [0x9A, 0x01, 0xF6, 0x01];

fn is_hello(w: &[u8]) -> bool {
    w[..HEADER] == HELLO_HEAD[..] && w[HEADER + 2..HELLO_LEN] == HELLO_GEOMETRY
}

/// Frames out of a byte stream that arrives in pieces.
///
/// A chip reset cuts the frame on the wire short, and the decoder would
/// wait for the rest of it - the next boot's frames, HELLO first - for
/// ever. So the bridge resets it whenever it resets the chip, and a HELLO
/// header among the bytes of a frame still waiting for its payload (a
/// panic's reboot, which nobody announces) starts the stream again there.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// How far into the waiting frame (at the front of buf) HELLO has
    /// been looked for already.
    scanned: usize,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget what is pending: the chip was reset, and the frame it was
    /// sending will never end.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.scanned = 0;
    }

    /// The frames `data` completes, oldest first.
    pub fn feed(&mut self, data: &[u8]) -> Vec<(u8, Vec<u8>)> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        let mut at = 0; // what of buf is consumed; drained once at the end
        loop {
            let rest = &self.buf[at..];
            let Some(i) = rest.windows(2).position(|w| w == MAGIC) else {
                // Keep a trailing `K`, which may be half of the next magic.
                at = if rest.last() == Some(&b'K') { self.buf.len() - 1 } else { self.buf.len() };
                break;
            };
            at += i;
            let rest = &self.buf[at..];
            if rest.len() < HEADER {
                break;
            }
            let type_ = rest[2];
            let len = u32::from_le_bytes([rest[3], rest[4], rest[5], rest[6]]) as usize;
            if len > MAX_LEN {
                // Not a frame we can trust: step past this magic and look again.
                at += 2;
                self.scanned = 0;
                continue;
            }
            if rest.len() < HEADER + len {
                // Still waiting: unless a boot began among what came.
                let from = self.scanned.saturating_sub(HELLO_LEN - 1).max(HEADER);
                if let Some(j) = rest.get(from..).unwrap_or(&[]).windows(HELLO_LEN).position(is_hello) {
                    at += from + j;
                    self.scanned = 0;
                    continue;
                }
                self.scanned = rest.len();
                break;
            }
            let payload = rest[HEADER..HEADER + len].to_vec();
            at += HEADER + len;
            self.scanned = 0;
            if type_ == FB_PACKED {
                // A corrupt picture costs that picture.
                if let Some(p) = unpack_rect(&payload) {
                    out.push((FB_RECT, p));
                }
                continue;
            }
            out.push((type_, payload));
        }
        self.buf.drain(..at);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16s(v: &[u16]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn rect_head(x: u16, y: u16, w: u16, h: u16) -> Vec<u8> {
        u16s(&[x, y, w, h])
    }

    #[test]
    fn roundtrip() {
        let frame = encode(LED, &[255, 0, 0]);
        assert_eq!(Decoder::new().feed(&frame), vec![(LED, vec![255, 0, 0])]);
    }

    #[test]
    fn split_reads() {
        let frame = encode(TOUCH, &[1, 10, 0, 20, 0]);
        let mut d = Decoder::new();
        let mut got = vec![];
        for b in frame {
            got.extend(d.feed(&[b]));
        }
        assert_eq!(got, vec![(TOUCH, vec![1, 10, 0, 20, 0])]);
    }

    #[test]
    fn garbage_resync() {
        let hello = b"\x01\x00\x9a\x01\xf6\x01".to_vec();
        let frame = encode(HELLO, &hello);
        let mut data = b"\x00\xffKX".to_vec();
        data.extend(&frame);
        data.extend(b"junk");
        data.extend(&frame);
        assert_eq!(Decoder::new().feed(&data), vec![(HELLO, hello.clone()), (HELLO, hello)]);
    }

    #[test]
    fn a_magic_split_across_reads_is_kept() {
        let frame = encode(LED, &[1, 2, 3]);
        let mut d = Decoder::new();
        let mut first = b"noise".to_vec();
        first.push(b'K');
        assert!(d.feed(&first).is_empty());
        assert_eq!(d.feed(&frame[1..]), vec![(LED, vec![1, 2, 3])]);
    }

    #[test]
    fn oversize_dropped() {
        let mut bad = b"KD".to_vec();
        bad.push(FB_RECT);
        bad.extend(((MAX_LEN + 1) as u32).to_le_bytes());
        bad.extend([0u8; 16]);
        bad.extend(encode(LED, &[1, 2, 3]));
        assert_eq!(Decoder::new().feed(&bad), vec![(LED, vec![1, 2, 3])]);
    }

    fn hello() -> Vec<u8> {
        encode(HELLO, &u16s(&[1, 410, 502]))
    }

    /// A screen frame the chip's reset cut short: its header, a little of
    /// its pixels, and never the rest.
    fn cut_frame() -> Vec<u8> {
        let mut cut = b"KD".to_vec();
        cut.push(FB_PACKED);
        cut.extend(500_000u32.to_le_bytes());
        cut.extend(rect_head(0, 0, 410, 502));
        cut.extend([0x81, 5, 0, 0x01]);
        cut
    }

    #[test]
    fn a_boot_after_a_cut_frame_is_seen() {
        // A panic reboot: nobody tells the decoder, the HELLO has to.
        let mut d = Decoder::new();
        assert!(d.feed(&cut_frame()).is_empty());
        let mut next = b"\x00ESP-ROM noise".to_vec();
        next.extend(hello());
        next.extend(encode(LED, &[1, 2, 3]));
        assert_eq!(d.feed(&next), vec![(HELLO, u16s(&[1, 410, 502])), (LED, vec![1, 2, 3])]);
    }

    #[test]
    fn hello_s_header_inside_a_payload_is_not_a_boot() {
        // An SD write's data that happens to hold HELLO's 7-byte header, but
        // not a boot's geometry: the frame it is in arrives whole.
        let mut data = vec![0u8; 64];
        data[10..17].copy_from_slice(b"KD\x01\x06\x00\x00\x00");
        data[17..23].copy_from_slice(&[9, 9, 9, 9, 9, 9]);
        let frame = encode(LED, &data);
        let mut d = Decoder::new();
        let mut got = d.feed(&frame[..30]);
        got.extend(d.feed(&frame[30..]));
        assert_eq!(got, vec![(LED, data)]);
    }

    #[test]
    fn a_boot_is_seen_when_it_arrives_a_byte_at_a_time() {
        let mut d = Decoder::new();
        let mut got = d.feed(&cut_frame());
        let mut next = hello();
        next.extend(encode(LED, &[1, 2, 3]));
        for b in next {
            got.extend(d.feed(&[b]));
        }
        assert_eq!(got, vec![(HELLO, u16s(&[1, 410, 502])), (LED, vec![1, 2, 3])]);
    }

    #[test]
    fn a_reset_forgets_the_frame_it_cut() {
        let mut d = Decoder::new();
        assert!(d.feed(&cut_frame()).is_empty());
        d.reset();
        assert_eq!(d.feed(&encode(LED, &[1, 2, 3])), vec![(LED, vec![1, 2, 3])]);
    }

    #[test]
    fn header_layout() {
        assert_eq!(encode(0x20, b"ab"), b"KD\x20\x02\x00\x00\x00ab".to_vec());
    }

    // The vectors firmware/tests/test_frame.c packs with kemu_pack16().
    #[test]
    fn unpack16_runs_and_literals() {
        let data = [0x81, 5, 0, 0x01, 1, 0, 2, 0, 0x80, 3, 0];
        assert_eq!(unpack16(&data), Some(u16s(&[5, 5, 5, 1, 2, 3, 3])));
    }

    #[test]
    fn unpack16_long_run() {
        let mut data = vec![];
        for _ in 0..2 {
            data.extend([0xFF, 0x34, 0x12]);
        }
        data.extend([0xA8, 0x34, 0x12]);
        assert_eq!(unpack16(&data), Some(u16s(&[0x1234; 300])));
    }

    #[test]
    fn unpack16_truncated_is_refused() {
        assert_eq!(unpack16(&[0x01, 1, 0]), None); // two literals promised, one there
        assert_eq!(unpack16(&[0x85, 1]), None); // a run with half its pixel
    }

    #[test]
    fn packed_rect_reaches_the_mac_as_a_rect() {
        let head = rect_head(3, 4, 7, 1);
        let mut packed = head.clone();
        packed.extend([0x81, 5, 0, 0x01, 1, 0, 2, 0, 0x80, 3, 0]);
        let mut want = head;
        want.extend(u16s(&[5, 5, 5, 1, 2, 3, 3]));
        assert_eq!(Decoder::new().feed(&encode(FB_PACKED, &packed)), vec![(FB_RECT, want)]);
    }

    #[test]
    fn packed_rect_of_the_wrong_size_is_dropped() {
        let mut packed = rect_head(0, 0, 8, 1); // 8 pixels promised, 7 packed
        packed.extend([0x81, 5, 0, 0x01, 1, 0, 2, 0, 0x80, 3, 0]);
        let mut data = encode(FB_PACKED, &packed);
        data.extend(encode(LED, &[1, 2, 3]));
        assert_eq!(Decoder::new().feed(&data), vec![(LED, vec![1, 2, 3])]);
    }
}

//! The ES8311's sample path, on esp-emu's I2S: the speaker the board plays
//! and the microphone it records.
//!
//! The chip's I2S unit runs its stock driver in esp-emu; this is the audio
//! side of the codec, on the data pads. What the chip sends ([`Codec::played`])
//! goes to the Mac's speaker as the board's one speaker plays it: the left
//! slot (the ES8311 has one DAC, which plays the left slot: REG09 bit 7 = 0,
//! which esp_codec_dev never sets), at the DAC's volume. What it reads
//! ([`Codec::supply`]) is what a board's microphone hears, three things
//! summed:
//!
//!  - the Mac's microphone, when the page has it on ([`Codec::heard`]);
//!  - the board's own speaker through the case, COUPLE_Q8/256 of what it
//!    played [`ECHO_NS`] before (-20 dB: a guess - nobody has measured a
//!    board's);
//!  - the ADC's floor, a few LSB of noise (about -85 dBFS): a real ADC never
//!    reads constant zeros, and kodeOS's audio test rightly calls that a dead
//!    I2S.
//!
//! The receiver is fed [`LEAD_NS`] ahead of emulated time, at every message
//! from esp-emu (its heartbeat comes every emulated millisecond). The lead is
//! small on purpose: kdrv_es8311 stops and starts the receiver with every
//! sound, esp-emu then drops what was queued and does not say so, and a small
//! lead is little to lose. The speaker is heard [`ECHO_NS`] late, the least
//! that lets its samples be in (esp-emu hands them over a DMA buffer, 5 ms,
//! at a time) when the microphone's are made, LEAD_NS ahead.

use std::collections::VecDeque;

use cosim::I2sFormat;

/// How far ahead of emulated time the receiver is fed.
pub const LEAD_NS: u64 = 5_000_000;
/// How late the microphone hears the speaker: the lead and a DMA buffer.
pub const ECHO_NS: u64 = LEAD_NS + 6_000_000;
/// Speaker to microphone: -20 dB.
const COUPLE_Q8: i32 = 26;
/// The ADC's own noise, +-3 LSB.
const FLOOR_LSB: u32 = 3;
/// Samples of the speaker per PCM_OUT frame.
const CHUNK: usize = 1024;
/// Further behind than this and the receiver is fed from now, not from where
/// it was: a jump of the clock is not a backlog to make up.
const BEHIND_NS: u64 = 200_000_000;

/// A sample's index at `rate`, counted from emulated time 0.
fn index(ns: u64, rate: u32) -> u64 {
    (ns as u128 * rate as u128 / 1_000_000_000) as u64
}

pub struct Codec {
    tx: Option<I2sFormat>,
    /// The receive format and the emulated time it was fed from.
    rx: Option<(I2sFormat, u64)>,
    /// Frames fed to the receiver since then.
    fed: u64,
    /// The speaker as the microphone hears it, by sample index at the
    /// transmit rate: (index, value). Half a second.
    echo: Vec<(u64, i16)>,
    /// The Mac's microphone, mono, at the receive rate.
    mic: VecDeque<i16>,
    noise: u32,
}

impl Default for Codec {
    fn default() -> Self {
        Self::new()
    }
}

impl Codec {
    pub fn new() -> Codec {
        Codec { tx: None, rx: None, fed: 0, echo: vec![], mic: VecDeque::new(), noise: 0x2545_F491 }
    }

    /// A format the chip programmed. The receiver's rate, when it is the
    /// receive format and new: what the Mac's microphone should send at.
    pub fn format(&mut self, f: I2sFormat, now_ns: u64) -> Option<u32> {
        if !f.receive {
            if self.tx != Some(f) {
                self.echo = vec![(u64::MAX, 0); (f.rate_hz / 2).max(1) as usize];
            }
            self.tx = Some(f);
            return None;
        }
        if self.rx.is_some_and(|(r, _)| r == f) {
            return None;
        }
        self.rx = Some((f, now_ns));
        self.fed = 0;
        self.mic.clear();
        Some(f.rate_hz)
    }

    /// What the chip played, its last sample clocked out at `now_ns`: PCM_OUT
    /// payloads for the page (rate u32, one channel, then the left slot at
    /// `gain_q8`, 256 = 0 dB), none for silence.
    pub fn played(&mut self, pcm: &[u8], gain_q8: i32, now_ns: u64) -> Vec<Vec<u8>> {
        let Some(f) = self.tx else { return vec![] };
        let width = (f.bits / 8) as usize;
        let frame = width * f.channels as usize;
        if width == 0 || frame == 0 {
            return vec![];
        }
        let left: Vec<i16> = pcm
            .chunks_exact(frame)
            .map(|fr| {
                // The top 16 bits of the container, little-endian.
                let hi = if width == 1 {
                    (fr[0] as i8 as i16) << 8
                } else {
                    i16::from_le_bytes([fr[width - 2], fr[width - 1]])
                };
                clamp16(hi as i32 * gain_q8 / 256)
            })
            .collect();
        let end = index(now_ns, f.rate_hz);
        let first = end.saturating_sub(left.len() as u64);
        let n = self.echo.len() as u64;
        for (i, &v) in left.iter().enumerate() {
            let at = first + i as u64;
            self.echo[(at % n) as usize] = (at, (v as i32 * COUPLE_Q8 / 256) as i16);
        }
        left.chunks(CHUNK)
            .filter(|c| c.iter().any(|&v| v != 0))
            .map(|c| {
                let mut p = Vec::with_capacity(5 + c.len() * 2);
                p.extend_from_slice(&f.rate_hz.to_le_bytes());
                p.push(1);
                for v in c {
                    p.extend_from_slice(&v.to_le_bytes());
                }
                p
            })
            .collect()
    }

    /// What the Mac's microphone heard, mono at the receive rate.
    pub fn heard(&mut self, mono: &[i16]) {
        let Some((f, _)) = self.rx else { return };
        self.mic.extend(mono);
        // Half a second at most: older sound is latency, as a board's RX DMA
        // would have dropped it.
        let keep = f.rate_hz as usize / 2;
        if self.mic.len() > keep {
            self.mic.drain(..self.mic.len() - keep);
        }
    }

    /// The receiver's samples up to [`LEAD_NS`] past `now_ns`, in its
    /// format, or None when it has had them already (or has no format yet).
    pub fn supply(&mut self, now_ns: u64) -> Option<Vec<u8>> {
        let (f, since) = self.rx?;
        if now_ns < since || f.rate_hz == 0 || f.channels == 0 {
            return None;
        }
        let at = |ns: u64| index(ns, f.rate_hz);
        let due = at(now_ns - since + LEAD_NS);
        if due.saturating_sub(self.fed) > at(LEAD_NS + BEHIND_NS) {
            self.fed = due - at(LEAD_NS);
        }
        if due <= self.fed {
            return None;
        }
        // The speaker's index for the receiver's first new sample, ECHO_NS
        // earlier; only when both run at one rate.
        let echo_from = self
            .tx
            .filter(|t| t.rate_hz == f.rate_hz && !self.echo.is_empty())
            .map(|_| (at(since) + self.fed).checked_sub(at(ECHO_NS)));
        let n = (due - self.fed) as usize;
        self.fed = due;
        let width = (f.bits / 8) as usize;
        let mut out = Vec::with_capacity(n * width * f.channels as usize);
        for k in 0..n {
            let echo = match echo_from {
                Some(Some(e)) => {
                    let i = e + k as u64;
                    let (tag, v) = self.echo[(i % self.echo.len() as u64) as usize];
                    if tag == i {
                        v as i32
                    } else {
                        0
                    }
                }
                _ => 0,
            };
            let v = clamp16(self.mic.pop_front().unwrap_or(0) as i32 + echo + self.floor());
            put(&mut out, v, width);
            for _ in 1..f.channels {
                put(&mut out, 0, width);
            }
        }
        Some(out)
    }

    /// The chip reset at `now_ns`: a new timeline, and nothing queued.
    pub fn reset(&mut self, now_ns: u64) {
        if let Some((_, since)) = &mut self.rx {
            *since = now_ns;
        }
        self.fed = 0;
        self.mic.clear();
        self.echo.fill((u64::MAX, 0));
    }

    fn floor(&mut self) -> i32 {
        let x = &mut self.noise; // xorshift32
        *x ^= *x << 13;
        *x ^= *x >> 17;
        *x ^= *x << 5;
        (*x % (2 * FLOOR_LSB + 1)) as i32 - FLOOR_LSB as i32
    }
}

fn clamp16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// One sample in a container `width` bytes wide, little-endian, at the top.
fn put(out: &mut Vec<u8>, v: i16, width: usize) {
    let b = v.to_le_bytes();
    match width {
        1 => out.push(b[1]),
        2 => out.extend_from_slice(&b),
        w => {
            out.extend(std::iter::repeat_n(0, w - 2));
            out.extend_from_slice(&b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TX: I2sFormat = I2sFormat { receive: false, rate_hz: 48000, channels: 2, bits: 16 };
    const RX: I2sFormat = I2sFormat { receive: true, rate_hz: 48000, channels: 2, bits: 16 };
    /// Frames in a millisecond at 48 kHz.
    const MS: usize = 48;

    fn stereo(frames: &[(i16, i16)]) -> Vec<u8> {
        frames.iter().flat_map(|(l, r)| [l.to_le_bytes(), r.to_le_bytes()].concat()).collect()
    }

    fn samples(p: &[u8]) -> Vec<i16> {
        p.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect()
    }

    fn left(p: &[u8]) -> Vec<i16> {
        samples(p).into_iter().step_by(2).collect()
    }

    #[test]
    fn the_speaker_plays_the_left_slot_at_the_dac_volume() {
        let mut c = Codec::new();
        c.format(TX, 0);
        let out = c.played(&stereo(&[(1000, -7), (-2000, 7)]), 128, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(&out[0][..5], &[0x80, 0xbb, 0, 0, 1], "48000 Hz, one channel");
        assert_eq!(samples(&out[0][5..]), vec![500, -1000]);
    }

    #[test]
    fn silence_and_a_muted_dac_send_nothing_and_nothing_before_the_format() {
        let mut c = Codec::new();
        assert!(c.played(&stereo(&[(1000, 0)]), 256, 0).is_empty(), "no format yet");
        c.format(TX, 0);
        assert!(c.played(&stereo(&[(1000, 0)]), 0, 0).is_empty(), "muted");
        assert!(c.played(&stereo(&[(0, 5); 240]), 256, 0).is_empty(), "the left slot silent");
    }

    #[test]
    fn wide_containers_play_their_top_sixteen_bits() {
        let mut c = Codec::new();
        c.format(I2sFormat { bits: 32, ..TX }, 0);
        let pcm: Vec<u8> = [0x1234_5678i32, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(samples(&c.played(&pcm, 256, 0)[0][5..]), vec![0x1234]);
    }

    #[test]
    fn the_speaker_goes_out_in_frames_of_a_thousand_samples() {
        let mut c = Codec::new();
        c.format(TX, 0);
        let out = c.played(&stereo(&vec![(1, 1); 2500]), 256, 1_000_000_000);
        assert_eq!(out.iter().map(|p| (p.len() - 5) / 2).collect::<Vec<_>>(), vec![1024, 1024, 452]);
    }

    #[test]
    fn the_receiver_is_fed_ahead_of_emulated_time_once_and_no_more() {
        let mut c = Codec::new();
        assert_eq!(c.supply(1_000_000), None, "no receive format yet");
        assert_eq!(c.format(RX, 1_000_000), Some(48000));
        assert_eq!(c.supply(1_000_000).unwrap().len(), 5 * MS * 4, "5 ms of stereo 16-bit");
        assert_eq!(c.supply(1_000_000), None, "already fed");
        assert_eq!(c.supply(2_000_000).unwrap().len(), MS * 4, "a millisecond more");
        assert_eq!(c.format(RX, 20_000_000), None, "the same format again is not news");
    }

    #[test]
    fn a_quiet_desk_is_the_adc_floor_in_the_left_slot() {
        let mut c = Codec::new();
        c.format(RX, 0);
        let s = samples(&c.supply(0).unwrap());
        let l: Vec<i16> = s.iter().step_by(2).copied().collect();
        assert!(l.iter().all(|v| v.abs() <= 3), "a few LSB");
        assert!(l.iter().any(|&v| v != 0), "never constant zeros");
        assert!(s.iter().skip(1).step_by(2).all(|&v| v == 0), "the right slot is empty");
    }

    #[test]
    fn the_microphone_hears_the_mac() {
        let mut c = Codec::new();
        c.format(RX, 0);
        c.heard(&[10_000; 10]);
        let l = left(&c.supply(0).unwrap());
        assert!(l[..10].iter().all(|v| (10_000 - 3..=10_000 + 3).contains(&(*v as i32))), "{:?}", &l[..10]);
        assert!(l[10].abs() <= 3, "then the floor alone");
    }

    #[test]
    fn the_microphone_hears_the_speaker_echo_ns_after_it_played() {
        // The speaker plays 25600 (-> 2600 in the case) from 100 ms to 105 ms;
        // the receiver, fed from 0, hears it from 111 ms to 116 ms.
        let mut c = Codec::new();
        c.format(TX, 0);
        c.format(RX, 0);
        let mut heard = vec![];
        for ms in 0..=130u64 {
            if ms == 105 {
                c.played(&stereo(&[(25_600, 0); 5 * MS]), 256, 105_000_000);
            }
            if let Some(p) = c.supply(ms * 1_000_000) {
                heard.extend(left(&p));
            }
        }
        let loud: Vec<usize> = (0..heard.len()).filter(|&i| heard[i] > 2000).collect();
        assert_eq!((loud[0], loud.len()), (111 * MS, 5 * MS), "heard at 111 ms, for 5 ms");
    }

    #[test]
    fn a_clock_that_jumps_is_not_a_backlog() {
        let mut c = Codec::new();
        c.format(RX, 0);
        c.supply(0);
        assert_eq!(c.supply(10_000_000_000).unwrap().len(), 5 * MS * 4, "5 ms from now, not ten seconds");
    }

    #[test]
    fn a_reset_starts_a_new_timeline_with_nothing_queued() {
        let mut c = Codec::new();
        c.format(TX, 0);
        c.format(RX, 5_000_000_000);
        c.heard(&[10_000; 100]);
        c.played(&stereo(&[(25_600, 0); 1000]), 256, 5_020_000_000);
        c.reset(0);
        let l = left(&c.supply(0).unwrap());
        assert_eq!(l.len(), 5 * MS);
        assert!(l.iter().all(|v| v.abs() <= 3));
    }
}

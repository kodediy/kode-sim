//! esp-emu's co-simulation bus, as a device outside the emulator speaks it
//! over `--cosim-tcp` (esp-emulator's proto/esp_cosim.proto and
//! docs/guides/HOST-INTERFACES.md).
//!
//! Only the messages a board of devices needs, encoded by hand as
//! esp-emulator's own tools/cosim_tcp.py does: register its claims, drive
//! pins, answer I2C transactions, feed an I2S receiver; and read back the
//! heartbeat, pin levels, bus requests, the registration's answer, chip
//! resets and what an I2S unit plays.
//!
//! Frames are `[len: u32 LE][frame]` both ways; from the emulator a frame is
//! `[topic_len: u16 LE][topic][EmulatorMessage]`, to it a bare message.

use std::io::{self, Read, Write};

pub mod client;
pub use client::Client;

/// What a device claims when it registers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Claim {
    /// Pins it drives (exclusive) and pins whose every transition it hears.
    Gpio { drive: u64, watch: u64 },
    /// The device at this 7-bit address, on these two pads.
    I2c { addr: u8, scl: u8, sda: u8 },
    /// The audio device on these I2S data pads, named from the chip's end:
    /// it hears what the chip sends on `dout` and feeds what it reads on
    /// `din`.
    I2s { dout: Option<u8>, din: Option<u8> },
    /// The panel on this virtual channel of the MIPI-DSI link (0..3): it
    /// hears the commands the firmware sends it and the frames it shows.
    Dsi { vc: u8 },
    /// The computer on the other end of a USB-OTG port's cable (ESP32-P4: 0
    /// high speed, 1 full speed): the emulator's own host leaves the port,
    /// [`Payload::UsbLink`] says when a device is there, and
    /// [`usb_transfer`] runs transfers on it (esp-emu 1.3.0).
    UsbHost { port: u8 },
}

/// One transfer the USB host runs (UsbTransfer): a control transfer with
/// its 8-byte `setup` on endpoint 0, else `data` out, or up to `length`
/// bytes in (bit 7 of `endpoint`). Each is answered by a
/// [`Payload::UsbResult`] that quotes `id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbTransfer {
    pub id: u64,
    pub endpoint: u8,
    pub setup: Vec<u8>,
    pub data: Vec<u8>,
    pub length: u32,
    /// How long it may wait on an endpoint the firmware has not armed, in
    /// microseconds of emulated time; 0, until the device leaves.
    pub timeout_us: u32,
}

/// How a [`UsbTransfer`] ended (UsbStatus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbStatus {
    Ok,
    /// The endpoint answered STALL.
    Stall,
    /// `timeout_us` passed with the endpoint not armed.
    Timeout,
    /// No device: detached, in reset, or gone meanwhile.
    NoDevice,
    /// A packet that does not fit what was asked for.
    Overflow,
    /// Not run: no such endpoint, a bad SETUP, or too many waiting.
    Refused,
    Other(u64),
}

/// The pixels of a DSI video stream ([`Dsi::Format`]): the frame buffer's
/// own bytes, in memory order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DpiPixels {
    /// 16-bit RGB565 words, little-endian.
    Rgb565,
    /// Three bytes a pixel: blue, green, red.
    Bgr888,
    /// One this crate does not know.
    Other(u32),
}

/// What the DSI host sends the panel that claimed its virtual channel
/// (DsiEvent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dsi {
    /// One packet, as sent: the MIPI data type (0x05, 0x15, 0x39 for DCS
    /// writes) and its bytes, the command first.
    Command { dt: u8, data: Vec<u8> },
    /// The video stream's shape, when it changes.
    Format { streaming: bool, width: u32, height: u32, pixels: DpiPixels },
    /// Rows of frame `frame` that differ from what the panel last saw,
    /// `rows` whole rows from `y` in the stream's format.
    Rows { frame: u64, y: u32, rows: u32, pixels: Vec<u8> },
}

/// What the samples of one I2S direction look like (I2sFormat): sent when
/// it becomes known and whenever it changes, never per chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct I2sFormat {
    /// false: what the chip sends (I2sOut); true: what it reads (I2sIn).
    pub receive: bool,
    pub rate_hz: u32,
    /// Slots in a frame: 2 for standard stereo.
    pub channels: u32,
    /// Bits of one slot's container: 8, 16, 24 or 32.
    pub bits: u32,
}

/// One I2C transaction the chip waits on (BusRequest.i2c).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cRequest {
    /// The correlation id the answer echoes.
    pub id: u64,
    pub bus: u32,
    pub addr: u8,
    /// Every byte the master clocked out.
    pub write: Vec<u8>,
    /// Bytes it expects back; 0 for a write.
    pub read_len: usize,
    /// The next piece of a read the driver split.
    pub continues: bool,
}

/// What an EmulatorMessage from the emulator carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Payload {
    Clock,
    Level {
        pin: u8,
        level: bool,
    },
    Registered {
        accepted: bool,
        chip: String,
        reason: String,
    },
    I2c(I2cRequest),
    /// A bus request this crate does not answer (SPI).
    OtherBus {
        id: u64,
    },
    ChipReset {
        pin_levels: u64,
    },
    I2sFormat(I2sFormat),
    /// What the chip clocked out since the last one: frames of the transmit
    /// format, interleaved, little-endian.
    I2sOut(Vec<u8>),
    /// For the panel on a DSI virtual channel.
    Dsi(Dsi),
    /// The USB host's port: a device is there (its pull-up is on and its
    /// bus reset over) or not; sent once the claim is in and on each change.
    UsbLink {
        attached: bool,
        high_speed: bool,
    },
    /// How a [`UsbTransfer`] ended: what the device sent, and how many
    /// bytes of the transfer's data it took.
    UsbResult {
        id: u64,
        status: UsbStatus,
        data: Vec<u8>,
        sent: u32,
    },
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// The emulated time it happened at, in ns since the chip's reset.
    pub sim_time_ns: u64,
    pub payload: Payload,
}

// ---- protobuf, by hand ------------------------------------------------------

fn varint(out: &mut Vec<u8>, mut n: u64) {
    while n > 0x7F {
        out.push((n as u8 & 0x7F) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn vf(out: &mut Vec<u8>, field: u32, v: u64) {
    varint(out, (field as u64) << 3);
    varint(out, v);
}

fn ld(out: &mut Vec<u8>, field: u32, inner: &[u8]) {
    varint(out, ((field as u64) << 3) | 2);
    varint(out, inner.len() as u64);
    out.extend_from_slice(inner);
}

/// A message's fields as (number, value), in order; a varint's value is in
/// `.1`, a length-delimited one's bytes in `.2`.
fn fields(buf: &[u8]) -> Option<Vec<(u32, u64, &[u8])>> {
    fn read_varint(buf: &[u8], i: &mut usize) -> Option<u64> {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let b = *buf.get(*i)?;
            *i += 1;
            v |= ((b & 0x7F) as u64) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                return Some(v);
            }
            if shift > 63 {
                return None;
            }
        }
    }
    let (mut out, mut i) = (vec![], 0);
    while i < buf.len() {
        let tag = read_varint(buf, &mut i)?;
        let (num, wire) = ((tag >> 3) as u32, tag & 7);
        match wire {
            0 => out.push((num, read_varint(buf, &mut i)?, &buf[0..0])),
            1 => {
                out.push((num, 0, buf.get(i..i + 8)?));
                i += 8;
            }
            2 => {
                let n = read_varint(buf, &mut i)? as usize;
                out.push((num, 0, buf.get(i..i + n)?));
                i += n;
            }
            5 => {
                out.push((num, 0, buf.get(i..i + 4)?));
                i += 4;
            }
            _ => return None,
        }
    }
    Some(out)
}

// ---- To the emulator --------------------------------------------------------

/// EmulatorMessage.register: this device's name and claims.
pub fn register(name: &str, claims: &[Claim]) -> Vec<u8> {
    let mut reg = vec![];
    vf(&mut reg, 1, 1); // proto_version
    ld(&mut reg, 2, name.as_bytes());
    for c in claims {
        let mut inner = vec![];
        match *c {
            Claim::Gpio { drive, watch } => {
                let mut g = vec![];
                vf(&mut g, 1, drive);
                if watch != 0 {
                    vf(&mut g, 2, watch);
                }
                ld(&mut inner, 1, &g);
            }
            Claim::I2c { addr, scl, sda } => {
                let mut i2c = vec![];
                vf(&mut i2c, 1, addr as u64);
                vf(&mut i2c, 2, scl as u64);
                vf(&mut i2c, 3, sda as u64);
                ld(&mut inner, 2, &i2c);
            }
            Claim::I2s { dout, din } => {
                let mut i2s = vec![];
                if let Some(p) = dout {
                    vf(&mut i2s, 3, p as u64);
                }
                if let Some(p) = din {
                    vf(&mut i2s, 4, p as u64);
                }
                ld(&mut inner, 17, &i2s);
            }
            Claim::Dsi { vc } => {
                let mut dsi = vec![];
                vf(&mut dsi, 1, vc as u64);
                ld(&mut inner, 20, &dsi);
            }
            Claim::UsbHost { port } => {
                let mut usb = vec![];
                vf(&mut usb, 1, port as u64);
                ld(&mut inner, 24, &usb);
            }
        }
        ld(&mut reg, 3, &inner);
    }
    let mut out = vec![];
    ld(&mut out, 4, &reg);
    out
}

/// EmulatorMessage{sim_time_ns, gpio_drive}: `pin` to `level` at `at_ns`.
pub fn gpio_drive(at_ns: u64, pin: u8, level: bool) -> Vec<u8> {
    let mut g = vec![];
    vf(&mut g, 1, pin as u64);
    vf(&mut g, 2, level as u64);
    let mut out = vec![];
    vf(&mut out, 1, at_ns);
    ld(&mut out, 6, &g);
    out
}

/// EmulatorMessage.bus_reply to an I2C request: the bytes read back (empty
/// for a write), or None for a NACK.
pub fn i2c_reply(id: u64, data: Option<&[u8]>) -> Vec<u8> {
    let mut r = vec![];
    vf(&mut r, 1, id);
    vf(&mut r, 2, data.is_some() as u64);
    if let Some(d) = data.filter(|d| !d.is_empty()) {
        ld(&mut r, 3, d);
    }
    vf(&mut r, 4, 1); // BUS_OP_I2C
    let mut out = vec![];
    ld(&mut out, 8, &r);
    out
}

/// EmulatorMessage.i2s_in: samples for the chip's receiver, in the layout
/// its receive [`I2sFormat`] gave.
pub fn i2s_in(pcm: &[u8]) -> Vec<u8> {
    let mut i = vec![];
    ld(&mut i, 1, pcm);
    let mut out = vec![];
    ld(&mut out, 31, &i);
    out
}

/// EmulatorMessage{sim_time_ns, usb_transfer}: `t`, started at `at_ns`
/// (a time already past starts it at once). Needs [`Claim::UsbHost`].
pub fn usb_transfer(at_ns: u64, t: &UsbTransfer) -> Vec<u8> {
    let mut u = vec![];
    vf(&mut u, 1, t.id);
    vf(&mut u, 2, t.endpoint as u64);
    if !t.setup.is_empty() {
        ld(&mut u, 3, &t.setup);
    }
    if !t.data.is_empty() {
        ld(&mut u, 4, &t.data);
    }
    if t.length != 0 {
        vf(&mut u, 5, t.length as u64);
    }
    if t.timeout_us != 0 {
        vf(&mut u, 6, t.timeout_us as u64);
    }
    let mut out = vec![];
    vf(&mut out, 1, at_ns);
    ld(&mut out, 44, &u);
    out
}

// ---- From the emulator ------------------------------------------------------

/// An EmulatorMessage, or None when it is not well formed.
pub fn decode(buf: &[u8]) -> Option<Message> {
    let mut m = Message { sim_time_ns: 0, payload: Payload::Other };
    for (num, v, b) in fields(buf)? {
        match num {
            1 => m.sim_time_ns = v,
            2 => m.payload = Payload::Clock,
            3 => {
                let (mut pin, mut level) = (0, false);
                for (n, v, _) in fields(b)? {
                    match n {
                        1 => pin = v as u8,
                        2 => level = v != 0,
                        _ => {}
                    }
                }
                m.payload = Payload::Level { pin, level };
            }
            5 => {
                let (mut accepted, mut chip, mut reason) = (false, String::new(), String::new());
                for (n, v, b) in fields(b)? {
                    match n {
                        2 => chip = String::from_utf8_lossy(b).into_owned(),
                        5 => accepted = v != 0,
                        6 => reason = String::from_utf8_lossy(b).into_owned(),
                        _ => {}
                    }
                }
                m.payload = Payload::Registered { accepted, chip, reason };
            }
            7 => {
                let mut id = 0;
                let mut i2c = None;
                for (n, v, b) in fields(b)? {
                    match n {
                        1 => id = v,
                        2 => {
                            let mut t =
                                I2cRequest { id: 0, bus: 0, addr: 0, write: vec![], read_len: 0, continues: false };
                            for (n, v, b) in fields(b)? {
                                match n {
                                    1 => t.bus = v as u32,
                                    2 => t.addr = v as u8,
                                    3 => t.write = b.to_vec(),
                                    4 => t.read_len = v as usize,
                                    5 => t.continues = v != 0,
                                    _ => {}
                                }
                            }
                            i2c = Some(t);
                        }
                        _ => {}
                    }
                }
                m.payload = match i2c {
                    Some(t) => Payload::I2c(I2cRequest { id, ..t }),
                    None => Payload::OtherBus { id },
                };
            }
            9 => {
                let pin_levels = fields(b)?.into_iter().find(|f| f.0 == 1).map_or(0, |f| f.1);
                m.payload = Payload::ChipReset { pin_levels };
            }
            29 => {
                let mut f = I2sFormat { receive: false, rate_hz: 0, channels: 0, bits: 0 };
                for (n, v, _) in fields(b)? {
                    match n {
                        1 => f.receive = v != 0,
                        2 => f.rate_hz = v as u32,
                        3 => f.channels = v as u32,
                        4 => f.bits = v as u32,
                        _ => {}
                    }
                }
                m.payload = Payload::I2sFormat(f);
            }
            30 => {
                let pcm = fields(b)?.into_iter().find(|f| f.0 == 1).map_or(vec![], |f| f.2.to_vec());
                m.payload = Payload::I2sOut(pcm);
            }
            36 => m.payload = Payload::Dsi(dsi_event(b)?),
            43 => {
                let f = fields(b)?;
                let varint = |k: u32| f.iter().find(|x| x.0 == k).map_or(0, |x| x.1);
                m.payload = Payload::UsbLink { attached: varint(1) != 0, high_speed: varint(2) == 2 };
            }
            45 => {
                let f = fields(b)?;
                let varint = |k: u32| f.iter().find(|x| x.0 == k).map_or(0, |x| x.1);
                let status = match varint(2) {
                    1 => UsbStatus::Ok,
                    2 => UsbStatus::Stall,
                    3 => UsbStatus::Timeout,
                    4 => UsbStatus::NoDevice,
                    5 => UsbStatus::Overflow,
                    6 => UsbStatus::Refused,
                    o => UsbStatus::Other(o),
                };
                let data = f.iter().find(|x| x.0 == 3).map_or(vec![], |x| x.2.to_vec());
                m.payload = Payload::UsbResult { id: varint(1), status, data, sent: varint(4) as u32 };
            }
            _ => {}
        }
    }
    Some(m)
}

/// A DsiEvent's one field.
fn dsi_event(b: &[u8]) -> Option<Dsi> {
    let (n, _, inner) = *fields(b)?.first()?;
    let f = fields(inner)?;
    let varint = |k: u32| f.iter().find(|x| x.0 == k).map_or(0, |x| x.1);
    let bytes = |k: u32| f.iter().find(|x| x.0 == k).map_or(vec![], |x| x.2.to_vec());
    Some(match n {
        1 => Dsi::Command { dt: varint(1) as u8, data: bytes(2) },
        2 => Dsi::Format {
            streaming: varint(1) != 0,
            width: varint(2) as u32,
            height: varint(3) as u32,
            pixels: match varint(4) {
                1 => DpiPixels::Rgb565,
                2 => DpiPixels::Bgr888,
                o => DpiPixels::Other(o as u32),
            },
        },
        3 => Dsi::Rows { frame: varint(1), y: varint(2) as u32, rows: varint(3) as u32, pixels: bytes(4) },
        _ => return None,
    })
}

// ---- Framing ----------------------------------------------------------------

/// A frame to the emulator.
pub fn write_frame(w: &mut impl Write, msg: &[u8]) -> io::Result<()> {
    let mut f = Vec::with_capacity(4 + msg.len());
    f.extend_from_slice(&(msg.len() as u32).to_le_bytes());
    f.extend_from_slice(msg);
    w.write_all(&f)
}

/// The next frame from the emulator as (topic, message); None once it has
/// gone.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(String, Vec<u8>)>> {
    let mut head = [0u8; 4];
    match r.read_exact(&mut head) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        other => other?,
    }
    let mut f = vec![0u8; u32::from_le_bytes(head) as usize];
    r.read_exact(&mut f)?;
    if f.len() < 2 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame without a topic"));
    }
    let tl = u16::from_le_bytes([f[0], f[1]]) as usize;
    let topic = f.get(2..2 + tl).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "topic past the frame"))?;
    Ok(Some((String::from_utf8_lossy(topic).into_owned(), f[2 + tl..].to_vec())))
}

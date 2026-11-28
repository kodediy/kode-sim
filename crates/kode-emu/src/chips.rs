//! The Kode Dot's chips beside an esp-emu, over `--cosim-tcp`: one device
//! that answers the board's seven I2C addresses, drives the pins they drive
//! on the P4 (the expander's INT, TOP) and is the ES8311's audio side on the
//! I2S data pads, and the AMOLED on the MIPI-DSI link, so unchanged firmware
//! finds the board it was built for.

use std::io;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use cosim::{gpio_drive, i2c_reply, i2s_in, register, Claim, Client, Payload};
use kodedot_chips::{Battery, Board, Event, Motion, PIN_BUTTON_TOP, PIN_IOEXP_INT};

use crate::audio::Codec;
use crate::panel::{Panel, Shown};

/// The board's I2C pads (kbsp_pins.h).
const SDA: u8 = 4;
const SCL: u8 = 5;
/// The ST25R3916B's IRQ: nothing models the NFC reader, so its line is held
/// at rest, or its interrupt task spins on a floating pin.
const PIN_NFC_IRQ: u8 = 29;
/// The ES8311's I2S data pads (kbsp_pins.h), from the P4's end: DSDIN, the
/// one the P4 sends on, and ASDOUT, the one it reads.
const PIN_I2S_DOUT: u8 = 38;
const PIN_I2S_DIN: u8 = 33;

/// What the board does that someone outside it sees or hears.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// A chip's doing: the LED, a buzz, the board off.
    Chip(Event),
    /// The speaker: a PCM_OUT payload (rate u32, one channel, i16 samples).
    Sound(Vec<u8>),
    /// The microphone's rate, once the firmware started the receiver at it.
    MicRate(u32),
    /// What the panel shows: a band of the picture, or its state.
    Screen(Shown),
    /// The chip reset: the firmware boots again (kemu or not).
    Reset,
}

/// What happens outside the bus, for whoever shows it (a page, a log).
pub type Sink = Box<dyn FnMut(Out) + Send>;

/// The board and the ends of its socket, shared by the thread answering
/// the bus and the [`Handle`]s changing it from outside.
struct Shared {
    board: Mutex<Board>,
    sender: cosim::client::Sender,
    /// The emulated time of the last message, which pin drives from
    /// outside the bus are stamped with.
    now_ns: AtomicU64,
    sink: Mutex<Sink>,
    /// The codec's DAC volume as last raised, 256 = 0 dB; 0 (silent) until
    /// the firmware sets it.
    gain: AtomicI32,
    codec: Mutex<Codec>,
    panel: Mutex<Panel>,
}

impl Shared {
    fn board(&self) -> MutexGuard<'_, Board> {
        self.board.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the chips raised, whoever made them raise it: each pin they
    /// drive to the emulator, and every event to the sink.
    fn flush(&self, board: &mut Board) -> io::Result<()> {
        let now = self.now_ns.load(Ordering::SeqCst);
        let mut sink = self.sink.lock().unwrap_or_else(PoisonError::into_inner);
        for e in board.take_events() {
            match e {
                Event::Pin { pin, level } => {
                    self.sender.send(&gpio_drive(now, pin, level))?;
                    continue;
                }
                Event::Gain(q8) => {
                    self.gain.store(q8, Ordering::SeqCst);
                    continue;
                }
                _ => {}
            }
            sink(Out::Chip(e));
        }
        Ok(())
    }

    fn codec(&self) -> MutexGuard<'_, Codec> {
        self.codec.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct Chips {
    shared: Arc<Shared>,
    client: Client,
}

impl Chips {
    /// Connect to the emulator's `--cosim-tcp` and register as the board.
    /// esp-emu does not wait for its devices: whoever attaches them resets
    /// the chip once this returns, so the firmware finds them from its
    /// first instruction.
    pub fn attach(cosim: &str, sink: Sink) -> io::Result<Chips> {
        let mut client = Client::connect(cosim, Duration::from_secs(10))?;
        let mut board = Board::new();
        let mut claims =
            vec![Claim::Gpio { drive: (1 << PIN_IOEXP_INT) | (1 << PIN_BUTTON_TOP) | (1 << PIN_NFC_IRQ), watch: 0 }];
        claims.extend(board.addrs().into_iter().map(|addr| Claim::I2c { addr, scl: SCL, sda: SDA }));
        claims.push(Claim::I2s { dout: Some(PIN_I2S_DOUT), din: Some(PIN_I2S_DIN) });
        claims.push(Claim::Dsi { vc: 0 });
        client.send(&register("kodedot", &claims))?;
        client.send(&gpio_drive(0, PIN_NFC_IRQ, false))?;
        client.send(&gpio_drive(0, PIN_BUTTON_TOP, true))?;
        loop {
            match client.next_message()? {
                None => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the emulator left before answering")),
                Some(m) => {
                    if let Payload::Registered { accepted, chip, reason } = m.payload {
                        if !accepted {
                            return Err(io::Error::other(format!("esp-emu refused the board: {reason}")));
                        }
                        if chip != "esp32p4" {
                            return Err(io::Error::other(format!("the Kode Dot is an esp32p4, not {chip}")));
                        }
                        break;
                    }
                }
            }
        }
        let shared = Arc::new(Shared {
            board: Mutex::new(board),
            sender: client.sender(),
            now_ns: AtomicU64::new(0),
            sink: Mutex::new(sink),
            gain: AtomicI32::new(0),
            codec: Mutex::new(Codec::new()),
            panel: Mutex::new(Panel::default()),
        });
        // The pins the chips drive from power-on (the expander's INT at rest).
        shared.flush(&mut shared.board())?;
        Ok(Chips { shared, client })
    }

    /// The board, to press buttons and set the battery from elsewhere.
    pub fn board(&self) -> Handle {
        Handle { shared: self.shared.clone() }
    }

    /// Answer the bus until the emulator goes.
    pub fn run(mut self) -> io::Result<()> {
        while let Some(m) = self.client.next_message()? {
            let now = m.sim_time_ns;
            self.shared.now_ns.store(now, Ordering::SeqCst);
            let mut board = self.shared.board();
            board.set_now_us(now / 1000);
            match m.payload {
                Payload::I2c(t) => {
                    let answer = board.i2c(t.addr, &t.write, t.read_len, t.continues);
                    self.shared.sender.send(&i2c_reply(t.id, answer.as_deref()))?;
                }
                // The board did not reset, nor do its chips; the codec's
                // queues belong to the old timeline.
                // The panel did not reset either: it keeps its picture, and
                // whether it is lit, as kodeOS's restart relies on.
                Payload::ChipReset { .. } => {
                    self.shared.codec().reset(now);
                    (self.shared.sink.lock().unwrap_or_else(PoisonError::into_inner))(Out::Reset);
                }
                Payload::Dsi(e) => {
                    let shown = self.shared.panel.lock().unwrap_or_else(PoisonError::into_inner).event(e);
                    if let Some(s) = shown {
                        (self.shared.sink.lock().unwrap_or_else(PoisonError::into_inner))(Out::Screen(s));
                    }
                }
                Payload::I2sFormat(f) => {
                    if let Some(rate) = self.shared.codec().format(f, now) {
                        (self.shared.sink.lock().unwrap_or_else(PoisonError::into_inner))(Out::MicRate(rate));
                    }
                }
                Payload::I2sOut(pcm) => {
                    let gain = self.shared.gain.load(Ordering::SeqCst);
                    let sound = self.shared.codec().played(&pcm, gain, now);
                    let mut sink = self.shared.sink.lock().unwrap_or_else(PoisonError::into_inner);
                    for p in sound {
                        sink(Out::Sound(p));
                    }
                }
                _ => {}
            }
            self.shared.flush(&mut board)?;
            drop(board);
            // Every message moves emulated time on: the receiver is kept fed.
            if let Some(pcm) = self.shared.codec().supply(now) {
                self.shared.sender.send(&i2s_in(&pcm))?;
            }
        }
        Ok(())
    }
}

/// Changes from outside the bus: the page's buttons, finger, battery and motion.
#[derive(Clone)]
pub struct Handle {
    shared: Arc<Shared>,
}

impl Handle {
    pub fn set_buttons(&self, mask: u8) -> io::Result<()> {
        let mut b = self.shared.board();
        b.set_buttons(mask);
        self.shared.flush(&mut b)
    }

    pub fn set_touch(&self, pressed: bool, x: u16, y: u16) -> io::Result<()> {
        let mut b = self.shared.board();
        b.set_touch(pressed, x, y);
        self.shared.flush(&mut b)
    }

    pub fn set_battery(&self, battery: Battery) -> io::Result<()> {
        let mut b = self.shared.board();
        b.set_battery(battery.percent, battery.usb);
        self.shared.flush(&mut b)
    }

    /// What the motion sensors measure.
    pub fn set_motion(&self, motion: Motion) -> io::Result<()> {
        let mut b = self.shared.board();
        b.set_motion(motion);
        self.shared.flush(&mut b)
    }

    /// What the Mac's microphone heard: mono i16 samples, little-endian, at
    /// the rate of [`Out::MicRate`].
    pub fn heard(&self, pcm: &[u8]) {
        let s: Vec<i16> = pcm.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
        self.shared.codec().heard(&s);
    }

    /// The emulated time, in ns since the chip's reset, as of the last
    /// message from the emulator: what a review's holds and waits count in.
    pub fn now_ns(&self) -> u64 {
        self.shared.now_ns.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// An emulator's frame: length, topic, message.
    fn from_emulator(msg: &[u8]) -> Vec<u8> {
        let topic = b"@tcp:1";
        let mut f = ((2 + topic.len() + msg.len()) as u32).to_le_bytes().to_vec();
        f.extend_from_slice(&(topic.len() as u16).to_le_bytes());
        f.extend_from_slice(topic);
        f.extend_from_slice(msg);
        f
    }

    #[test]
    fn the_board_keeps_the_emulated_time_its_messages_carry() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let emulator = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            // register_ack{proto 1, chip "esp32p4", accepted}
            s.write_all(&from_emulator(&[
                0x2a, 0x0d, 0x08, 0x01, 0x12, 0x07, b'e', b's', b'p', b'3', b'2', b'p', b'4', 0x28, 0x01,
            ]))
            .unwrap();
            // sim_time 1_000_000_000 (varint), gpio_level{pin 35, level 1}
            s.write_all(&from_emulator(&[0x08, 0x80, 0x94, 0xeb, 0xdc, 0x03, 0x1a, 0x04, 0x08, 0x23, 0x10, 0x01]))
                .unwrap();
            // What the board sent (its claims, its pins), read before it
            // goes: a socket closed on unread bytes is reset, not ended.
            s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
            let mut sent = [0u8; 4096];
            while matches!(std::io::Read::read(&mut s, &mut sent), Ok(n) if n > 0) {}
        });
        let chips = Chips::attach(&addr, Box::new(|_| {})).unwrap();
        let clock = chips.board();
        assert_eq!(clock.now_ns(), 0);
        chips.run().unwrap();
        assert_eq!(clock.now_ns(), 1_000_000_000);
        emulator.join().unwrap();
    }
}

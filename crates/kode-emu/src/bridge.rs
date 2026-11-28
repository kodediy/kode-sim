//! One running Kode Dot, served to any number of pages.
//!
//! The bridge owns esp-emu: it starts it, reads its UART1 and its console,
//! and keeps what a page needs to draw the device from scratch - the
//! screen, the panel, the LED, the battery and the power state - so that a
//! page that connects late, or reloads, is sent all of it at once.
//!
//! Page to bridge: binary KD frames for the Kode Dot's chips (TOUCH,
//! BUTTONS, BATTERY, and PCM_IN for the codec's microphone), and JSON
//! `{"console_in": line}`, `{"power": "on"|"reset"}`, `{"sd": bool}`,
//! `{"mic": bool}` (the Mac's microphone on or off, for every page).
//! BOT and PAD_LEFT held together for [`RESET_HOLD`] reset the chip, as the
//! board's reset circuit does.
//!
//! Bridge to page: the firmware's frames as they come - the chips' LED,
//! HAPTIC and POWER_OFF, the speaker's PCM_OUT, MIC_START and MIC_STOP among
//! them - and JSON `{"console": line}`,
//! `{"state": ..., "detail": ...}`, `{"battery": {...}}`, `{"sd": bool}`.
//!
//! Every method is short and never waits on a page: the state sits behind
//! one mutex held for a few copies at a time, and each page has a queue of
//! its own (see [`Page`]).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use kodedot_chips::{Battery, Event, Motion};

use crate::chips::Out;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::emu::{self, Emu, Input, Launch};
use crate::install::{self, InstallError, LinesPort};
use crate::proto;
use crate::sdcard::Card;
use crate::serial::Serial;
use crate::usbhost::{Cmd as UsbCmd, UsbHost};

pub const W: usize = 410;
pub const H: usize = 502;
/// The last console lines kept for a crash report.
const CRASH_LINES: usize = 40;
/// BOT + PAD_LEFT: ~1 s resets the P4 (kode-sdk's kqp_config.h).
pub const RESET_HOLD: Duration = Duration::from_secs(1);
const RESET_COMBO: u8 = kodedot_chips::BTN_BOT | kodedot_chips::BTN_LEFT;
/// Console lines a page that connects late is sent.
const CONSOLE_KEPT: usize = 500;
/// Screen and sound waiting for one page before it skips them: some 20
/// whole screens.
const MEDIA_BYTES: usize = 8 << 20;
/// Everything else waiting for one page - the console, mostly - before
/// it is let go: tens of thousands of lines, far more than a page that is
/// only slow ever has waiting.
const BACKLOG_BYTES: usize = 4 << 20;
/// A page that takes nothing from its queue for this long, with messages
/// waiting, is let go: a dead connection, or a tab that will never read.
const STUCK: Duration = Duration::from_secs(120);

/// What a page may send the firmware, and how long each may be: anything
/// else is dropped here. kemu's frame buffer is 8 KB, header included.
fn from_page_len(t: u8) -> Option<(usize, usize)> {
    match t {
        proto::TOUCH => Some((5, 5)),
        proto::BUTTONS => Some((1, 1)),
        proto::BATTERY => Some((2, 2)),
        proto::MOTION => Some((40, 40)),
        proto::PCM_IN => Some((2, 8192 - 7)),
        _ => None,
    }
}

/// The frames a page that falls behind may lose: it is sent the whole
/// picture again once it catches up, and a lost sound is a moment's gap.
fn droppable(m: &Msg) -> bool {
    matches!(m, Msg::Bin(b) if b.len() > 2 && (b[2] == proto::FB_RECT || b[2] == proto::PCM_OUT))
}

/// One message to a page.
#[derive(Clone, Debug)]
pub enum Msg {
    Text(String),
    Bin(Bytes),
}

impl Msg {
    fn len(&self) -> usize {
        match self {
            Msg::Text(t) => t.len(),
            Msg::Bin(b) => b.len(),
        }
    }
}

/// How much may wait for one page.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Bytes of screen and sound, past which they are skipped.
    pub media: usize,
    /// Bytes of everything else, past which the page is let go.
    pub backlog: usize,
    /// How long it may take nothing while something waits.
    pub stuck: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { media: MEDIA_BYTES, backlog: BACKLOG_BYTES, stuck: STUCK }
    }
}

/// What waits for one page, shared by the bridge's end and the page's.
struct Waiting {
    limits: Limits,
    media: AtomicUsize,
    backlog: AtomicUsize,
    depth: AtomicUsize,
    /// It skipped frames, and is owed the whole picture once caught up.
    behind: AtomicBool,
    /// When the queue last moved: something taken, or the first thing
    /// put in an empty one.
    moved: Mutex<Instant>,
    /// Set once the page is let go: its connection is closed, and the page
    /// reconnects to the whole picture.
    gone: tokio::sync::watch::Sender<bool>,
}

/// One connected page and the messages waiting for it.
///
/// Nothing the bridge does waits on a page: a message goes into the page's
/// queue and its own task sends it. A page that falls behind - a hidden
/// tab, a dead connection - loses screen and sound frames rather than
/// stalling the UART, which would stall the firmware behind it; when it
/// catches up it is sent the whole picture again, so what it shows is
/// never left half-drawn. Everything else (the state, the console, the
/// panel, the LED, the microphone) is always queued - up to [`Limits`]:
/// a page with more than that waiting, or that has taken nothing for too
/// long, is let go, and reconnects to the whole picture.
#[derive(Clone)]
pub struct Page {
    tx: mpsc::UnboundedSender<Msg>,
    w: Arc<Waiting>,
}

/// The receiving end a page's task sends from.
pub struct PageQueue {
    rx: mpsc::UnboundedReceiver<Msg>,
    w: Arc<Waiting>,
    gone: tokio::sync::watch::Receiver<bool>,
}

impl Page {
    pub fn new() -> (Page, PageQueue) {
        Page::with_limits(Limits::default())
    }

    pub fn with_limits(limits: Limits) -> (Page, PageQueue) {
        let (tx, rx) = mpsc::unbounded_channel();
        let w = Arc::new(Waiting {
            limits,
            media: AtomicUsize::new(0),
            backlog: AtomicUsize::new(0),
            depth: AtomicUsize::new(0),
            behind: AtomicBool::new(false),
            moved: Mutex::new(Instant::now()),
            gone: tokio::sync::watch::Sender::new(false),
        });
        let gone = w.gone.subscribe();
        (Page { tx, w: w.clone() }, PageQueue { rx, w, gone })
    }

    /// True once the page has been let go: its connection should close.
    pub fn let_go(&self) -> tokio::sync::watch::Receiver<bool> {
        self.w.gone.subscribe()
    }

    fn drop_page(&self) {
        self.w.gone.send_replace(true);
    }

    pub fn put(&self, m: Msg) {
        let w = &self.w;
        if *w.gone.borrow() {
            return;
        }
        let moved = || w.moved.lock().unwrap_or_else(PoisonError::into_inner);
        if w.depth.load(Ordering::SeqCst) > 0 && moved().elapsed() > w.limits.stuck {
            return self.drop_page();
        }
        let n = m.len();
        let media = droppable(&m);
        let (count, limit) = if media { (&w.media, w.limits.media) } else { (&w.backlog, w.limits.backlog) };
        if count.load(Ordering::SeqCst) + n > limit {
            if media {
                w.behind.store(true, Ordering::SeqCst);
            } else {
                self.drop_page();
            }
            return;
        }
        count.fetch_add(n, Ordering::SeqCst);
        if w.depth.fetch_add(1, Ordering::SeqCst) == 0 {
            *moved() = Instant::now();
        }
        if self.tx.send(m).is_err() {
            count.fetch_sub(n, Ordering::SeqCst);
            w.depth.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl PageQueue {
    /// The next message, and whether the page has just caught up after
    /// falling behind (so it is owed the whole picture once this is sent);
    /// None once the page is let go.
    pub async fn next(&mut self) -> Option<(Msg, bool)> {
        let m = tokio::select! {
            biased;
            _ = self.gone.wait_for(|g| *g) => return None,
            m = self.rx.recv() => m?,
        };
        let w = &self.w;
        let count = if droppable(&m) { &w.media } else { &w.backlog };
        count.fetch_sub(m.len(), Ordering::SeqCst);
        *w.moved.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
        let caught_up = w.depth.fetch_sub(1, Ordering::SeqCst) == 1 && w.behind.swap(false, Ordering::SeqCst);
        Some((m, caught_up))
    }
}

/// How this Kode Dot is run.
#[derive(Clone, Debug)]
pub struct Config {
    pub launch: Launch,
    pub http_port: u16,
    /// The card image, if there is a card.
    pub sd: Option<PathBuf>,
    /// Is it in the slot (the page's "SD insertada")?
    pub sd_in: bool,
}

/// MOTION's ten i32s.
fn motion_of(p: &[u8]) -> Motion {
    let v = |i: usize| i32::from_le_bytes([p[4 * i], p[4 * i + 1], p[4 * i + 2], p[4 * i + 3]]);
    Motion { accel_mg: [v(0), v(1), v(2)], gyro_mdps: [v(3), v(4), v(5)], mag_mg: [v(6), v(7), v(8)], temp_dc: v(9) }
}

/// The battery as the pages hear it.
fn battery_json(b: &Battery) -> Value {
    json!({"percent": b.percent, "usb": b.usb})
}

struct State {
    state: String,
    detail: String,
    fb: Vec<u8>,
    panel: Option<Vec<u8>>,
    led: Option<Vec<u8>>,
    /// The Mac's microphone (MIC_START's rate and channels), while a page
    /// has it on: a page that joins then is told, or it never sends it.
    mic: Option<Vec<u8>>,
    /// The rate the firmware's receiver last asked for.
    mic_rate: u32,
    battery: Battery,
    /// What the page last said the motion sensors measure: a new board is told it at boot.
    motion: Motion,
    pages: HashMap<u64, Page>,
    recent: VecDeque<String>,
    emu: Option<Arc<Emu>>,
    powering_off: bool,
    reset_combo: Option<JoinHandle<()>>,
    sd_in: bool,
    card: Option<Card>,
    /// The computer on the USB cable: its last snapshot, for a page that joins late.
    usb: Option<Value>,
    /// Installs under way, each hearing the console.
    taps: Vec<(u64, std::sync::mpsc::Sender<String>)>,
}

impl State {
    fn broadcast(&self, m: Msg) {
        for p in self.pages.values() {
            p.put(m.clone());
        }
    }

    fn broadcast_json(&self, j: Value) {
        self.broadcast(Msg::Text(j.to_string()));
    }

    /// The Mac's microphone on, at the receiver's rate, or off: every page
    /// is told, so the one with the microphone opens or shuts it and the
    /// rest show it.
    fn set_mic(&mut self, on: bool) {
        if on {
            let mut p = self.mic_rate.to_le_bytes().to_vec();
            p.push(1);
            self.broadcast(Msg::Bin(proto::encode(proto::MIC_START, &p).into()));
            self.mic = Some(p);
        } else {
            self.mic = None;
            self.broadcast(Msg::Bin(proto::encode(proto::MIC_STOP, &[]).into()));
        }
    }

    fn running_emu(&self) -> Option<&Arc<Emu>> {
        self.emu.as_ref().filter(|_| self.state == "running")
    }

    fn screen(&self) -> Msg {
        let mut p = Vec::with_capacity(8 + self.fb.len());
        for v in [0u16, 0, W as u16, H as u16] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        p.extend_from_slice(&self.fb);
        Msg::Bin(proto::encode(proto::FB_RECT, &p).into())
    }

    /// What a page draws the device from: the screen, the panel, the LED.
    fn picture(&self) -> Vec<Msg> {
        let mut out = vec![self.screen()];
        if let Some(p) = &self.panel {
            out.push(Msg::Bin(proto::encode(proto::PANEL, p).into()));
        }
        if let Some(l) = &self.led {
            out.push(Msg::Bin(proto::encode(proto::LED, l).into()));
        }
        out
    }

    /// The screen black and the panel unlit, on the pages already here too:
    /// a board coming up.
    fn dark(&mut self) {
        self.fb.fill(0);
        self.panel = Some(vec![0; 2]);
        self.broadcast(self.screen());
        self.broadcast(Msg::Bin(proto::encode(proto::PANEL, &[0; 2]).into()));
    }

    fn send_battery(&self) {
        if let Some(emu) = &self.emu {
            emu.input(Input::Battery(self.battery));
            emu.input(Input::Motion(self.motion));
        }
    }

    fn apply(&mut self, p: &[u8]) {
        if p.len() < 8 {
            return;
        }
        let v = |i: usize| u16::from_le_bytes([p[i], p[i + 1]]) as usize;
        let (x, y, w, h) = (v(0), v(2), v(4), v(6));
        if x + w > W || y + h > H || p.len() < 8 + w * h * 2 {
            return;
        }
        let body = &p[8..];
        for row in 0..h {
            let dst = ((y + row) * W + x) * 2;
            self.fb[dst..dst + w * 2].copy_from_slice(&body[row * w * 2..(row + 1) * w * 2]);
        }
    }
}

/// Is this the radio saying its C5 came up with no network in its vault
/// (kode-sdk's AO_Radio: "kaos.radio C5: KODE services v3, 0 networks, ...")?
fn first_join(line: &str) -> bool {
    line.contains("kaos.radio C5: KODE services") && line.contains(" 0 networks,")
}

/// What the terminal does with each console line.
pub type OnLine = Box<dyn Fn(&str) + Send>;

pub struct Bridge {
    pub cfg: Config,
    st: Mutex<State>,
    /// One boot at a time: two pages pressing at once, or a press while a
    /// boot is under way, must not start a second emulator.
    powering: tokio::sync::Mutex<()>,
    /// Set as [`Bridge::close`] begins: a boot under way gives up, and no
    /// other starts.
    closing: tokio::sync::watch::Sender<bool>,
    /// One install at a time, held by the work itself (not by the request
    /// that asked for it, which may go before the work does).
    installing: Arc<tokio::sync::Mutex<()>>,
    serial: Mutex<Option<Arc<Serial>>>,
    /// The computer at the other end of the USB cable, started with the first boot.
    usb: std::sync::OnceLock<UsbHost>,
    next_id: AtomicU64,
    /// How many pages have ever joined.
    joined: AtomicU64,
    /// What has crossed UART1 from the firmware, before the decoder: the
    /// simulator's bottleneck (api/stats).
    uart_bytes: AtomicU64,
    /// Each console line, for the terminal.
    on_line: Mutex<Option<OnLine>>,
    /// Each change of state, with its detail.
    on_state: Mutex<Option<mpsc::UnboundedSender<(String, String)>>>,
}

impl Bridge {
    pub fn new(cfg: Config) -> Arc<Bridge> {
        let sd_in = cfg.sd_in && cfg.sd.is_some();
        Arc::new(Bridge {
            cfg,
            st: Mutex::new(State {
                state: "off".into(),
                detail: String::new(),
                fb: vec![0; W * H * 2],
                panel: None,
                led: None,
                mic: None,
                mic_rate: 48000,
                battery: Battery { percent: 80, usb: true },
                motion: Motion::default(),
                pages: HashMap::new(),
                recent: VecDeque::new(),
                emu: None,
                powering_off: false,
                reset_combo: None,
                sd_in,
                card: None,
                usb: None,
                taps: vec![],
            }),
            powering: tokio::sync::Mutex::new(()),
            closing: tokio::sync::watch::Sender::new(false),
            installing: Arc::new(tokio::sync::Mutex::new(())),
            serial: Mutex::new(None),
            usb: std::sync::OnceLock::new(),
            next_id: AtomicU64::new(1),
            joined: AtomicU64::new(0),
            uart_bytes: AtomicU64::new(0),
            on_line: Mutex::new(None),
            on_state: Mutex::new(None),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn on_line(&self, f: OnLine) {
        *self.on_line.lock().unwrap_or_else(PoisonError::into_inner) = Some(f);
    }

    pub fn on_state(&self, tx: mpsc::UnboundedSender<(String, String)>) {
        *self.on_state.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
    }

    pub fn serial_path(&self) -> Option<String> {
        self.serial.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(|s| s.path.clone())
    }

    // ---- the emulator ---------------------------------------------------

    /// Boot, unless it is running already.
    pub async fn power_on(self: &Arc<Self>) -> Result<(), String> {
        let _one = self.powering.lock().await;
        if *self.closing.borrow() {
            return Err("kode-emu is stopping".into());
        }
        if self.lock().state == "running" {
            return Ok(());
        }
        let old = self.lock().emu.take();
        if let Some(old) = old {
            old.stop().await;
        }
        self.boot().await
    }

    async fn boot(self: &Arc<Self>) -> Result<(), String> {
        {
            let mut st = self.lock();
            st.fb.fill(0);
            st.panel = None;
            st.led = None;
            st.mic = None;
            st.powering_off = false;
            st.recent.clear();
        }
        // The card in the slot is the one there at power-on: esp-emu takes
        // no card in or out while it runs.
        let mut launch = self.cfg.launch.clone();
        launch.sd = self.cfg.sd.clone().filter(|_| self.lock().sd_in);
        let me = self.clone();
        // A launch given up half-way stops what it started (emu::launch).
        let mut closing = self.closing.subscribe();
        let (emu, wiring) = tokio::select! {
            launched = emu::launch(&launch, Arc::new(move |l| me.line(&l))) => launched?,
            _ = closing.wait_for(|c| *c) => return Err("kode-emu is stopping".into()),
        };
        self.lock().emu = Some(emu.clone());
        self.start_usb();
        self.spawn_pump(emu.clone(), wiring.uart);
        self.spawn_chips(emu.clone(), wiring.chips);
        let me = self.clone();
        let watched = emu.clone();
        tokio::spawn(async move {
            let rc = watched.wait().await;
            me.exited(&watched, rc);
        });
        self.set_state("running", "");
        self.lock().send_battery();
        Ok(())
    }

    /// The computer on the USB cable: once, for as long as the bridge lives.
    /// Its frames go to whichever emulator is running, and its snapshots to
    /// every page.
    fn start_usb(self: &Arc<Self>) {
        self.usb.get_or_init(|| {
            let wire_to = Arc::downgrade(self);
            let wire: crate::usbhost::Wire = Arc::new(move |t, p| {
                if let Some(me) = wire_to.upgrade() {
                    if let Some(emu) = me.lock().running_emu() {
                        emu.send(t, p);
                    }
                }
            });
            let emit_to = Arc::downgrade(self);
            let emit: crate::usbhost::Emit = Arc::new(move |j| {
                if let Some(me) = emit_to.upgrade() {
                    let mut st = me.lock();
                    st.usb = Some(j.clone());
                    st.broadcast_json(j);
                }
            });
            UsbHost::start(wire, emit)
        });
    }

    fn spawn_pump(self: &Arc<Self>, emu: Arc<Emu>, mut uart: tokio::net::tcp::OwnedReadHalf) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut dec = proto::Decoder::new();
            let mut resets = emu.resets();
            let mut buf = vec![0u8; 1 << 20];
            while let Ok(n) = uart.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                me.uart_bytes.fetch_add(n as u64, Ordering::Relaxed);
                if emu.resets() != resets {
                    // The chip was reset: the frame it was sending never ends.
                    resets = emu.resets();
                    dec.reset();
                }
                for (t, p) in dec.feed(&buf[..n]) {
                    me.from_firmware(t, p);
                }
            }
            drop(emu);
        });
    }

    /// The chips' LED, HAPTIC and POWER_OFF, and the speaker, as frames to
    /// the pages; the microphone's rate.
    fn spawn_chips(self: &Arc<Self>, emu: Arc<Emu>, mut events: mpsc::UnboundedReceiver<Out>) {
        let me = self.clone();
        tokio::spawn(async move {
            while let Some(o) = events.recv().await {
                let current = me.lock().emu.as_ref().is_some_and(|e| Arc::ptr_eq(e, &emu));
                if !current {
                    continue;
                }
                let frame = match o {
                    Out::Chip(Event::Led(rgb)) => (proto::LED, rgb.to_vec()),
                    Out::Chip(Event::Haptic { strength, ms }) => {
                        let mut p = vec![strength];
                        p.extend_from_slice(&ms.to_le_bytes());
                        (proto::HAPTIC, p)
                    }
                    Out::Chip(Event::PowerOff) => (proto::POWER_OFF, vec![]),
                    Out::Chip(_) => continue,
                    Out::Sound(p) => (proto::PCM_OUT, p),
                    Out::MicRate(rate) => {
                        me.mic_rate(rate);
                        continue;
                    }
                    Out::Screen(shown) => crate::panel::frame(&shown),
                    Out::Reset => {
                        emu.kemu_gone();
                        continue;
                    }
                };
                me.from_firmware(frame.0, frame.1);
            }
        });
    }

    /// The firmware's receiver runs at `rate`: the Mac's microphone, if a
    /// page has it on, is asked to send at it from now.
    fn mic_rate(&self, rate: u32) {
        let mut st = self.lock();
        let changed = st.mic_rate != rate;
        st.mic_rate = rate;
        if changed && st.mic.is_some() {
            st.set_mic(true);
        }
    }

    fn exited(&self, emu: &Arc<Emu>, rc: i32) {
        let (state, detail) = {
            let mut st = self.lock();
            if !st.emu.as_ref().is_some_and(|e| Arc::ptr_eq(e, emu)) {
                return;
            }
            st.mic = None;
            if let Some(u) = self.usb.get() {
                u.rebooted();
            }
            if st.powering_off {
                ("off", String::new())
            } else {
                let skip = st.recent.len().saturating_sub(CRASH_LINES);
                let last: Vec<&str> = st.recent.iter().skip(skip).map(String::as_str).collect();
                ("crashed", format!("esp-emu exited with {rc}\n{}", last.join("\n")))
            }
        };
        self.set_state(state, &detail);
    }

    /// One frame from the firmware (or from the chips, standing in for it).
    pub fn from_firmware(self: &Arc<Self>, t: u8, p: Vec<u8>) {
        // The USB cable's traffic is the computer's, not the pages'.
        if matches!(t, proto::USB_IN | proto::USB_DEV | proto::USB_ACK) {
            if let Some(u) = self.usb.get() {
                u.firmware_said(t, &p);
            }
            return;
        }
        let mut st = self.lock();
        match t {
            proto::HELLO => {
                if let Some(u) = self.usb.get() {
                    u.rebooted();
                }
                // A boot, the first or after a reset: the screen, the panel
                // and the LED start dark, as the board's do while it comes
                // up - on the pages already here too. The Mac's microphone
                // is the page's: it stays as it was.
                st.dark();
                st.led = Some(vec![0; 3]);
                st.broadcast(Msg::Bin(proto::encode(proto::LED, &[0; 3]).into()));
                if let Some(emu) = st.emu.clone() {
                    emu.kemu_seen();
                    // The P4's RTC keeps the wall clock across resets on the
                    // board; here it starts as an RTC that kept its supply
                    // would, at the Mac's time.
                    let us = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as u64;
                    emu.send(proto::TIME, &us.to_le_bytes());
                    st.send_battery();
                }
            }
            proto::FB_RECT => st.apply(&p),
            proto::PANEL => st.panel = Some(p.clone()),
            proto::LED => st.led = Some(p.clone()),
            proto::POWER_OFF => {
                st.powering_off = true;
                if let Some(emu) = st.emu.clone() {
                    tokio::spawn(async move { emu.stop().await });
                }
            }
            _ => {}
        }
        st.broadcast(Msg::Bin(proto::encode(t, &p).into()));
    }

    /// One console line: kept for late pages and crash reports, out of the
    /// serial port, to the terminal, to installs under way, to every page.
    pub fn line(&self, line: &str) {
        {
            let mut st = self.lock();
            st.recent.push_back(line.to_string());
            while st.recent.len() > CONSOLE_KEPT {
                st.recent.pop_front();
            }
            if let Some(s) = &*self.serial.lock().unwrap_or_else(PoisonError::into_inner) {
                s.write_line(line);
            }
            st.taps.retain(|(_, tx)| tx.send(line.to_string()).is_ok());
            st.broadcast_json(json!({"console": line}));
        }
        if let Some(f) = &*self.on_line.lock().unwrap_or_else(PoisonError::into_inner) {
            f(line);
        }
        if first_join(line) {
            // The C5's vault starts blank, as a new board's does: the person
            // joins the one network the simulator has once, as they would
            // their own. An app that asks for WiFi then gets an address, as
            // on a board that knows its network.
            self.line(&format!("[kode-emu] joining {} for you: the C5 knows no network yet", crate::c5::SSID));
            self.console_in(&format!("wifi_join ssid=\"{}\"", crate::c5::SSID));
        }
    }

    fn set_state(&self, state: &str, detail: &str) {
        {
            let mut st = self.lock();
            st.state = state.into();
            st.detail = detail.into();
            st.broadcast_json(json!({"state": state, "detail": detail}));
        }
        if let Some(tx) = &*self.on_state.lock().unwrap_or_else(PoisonError::into_inner) {
            let _ = tx.send((state.into(), detail.into()));
        }
    }

    // ---- the pages --------------------------------------------------------

    /// A page connecting: everything it needs to draw the device now, then
    /// whatever happens from here on. The id is for [`Bridge::leave`].
    pub fn join(&self, page: Page) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.joined.fetch_add(1, Ordering::SeqCst);
        let mut st = self.lock();
        page.put(Msg::Text(json!({"state": st.state, "detail": st.detail}).to_string()));
        page.put(Msg::Text(json!({"battery": battery_json(&st.battery)}).to_string()));
        page.put(Msg::Text(json!({"sd": st.sd_in}).to_string()));
        if let Some(u) = &st.usb {
            page.put(Msg::Text(u.to_string()));
        }
        for l in &st.recent {
            page.put(Msg::Text(json!({"console": l}).to_string()));
        }
        for m in st.picture() {
            page.put(m);
        }
        // Not part of the picture a page that fell behind is sent again:
        // that page never lost it, and a second MIC_START reopens its mic.
        if let Some(mic) = &st.mic {
            page.put(Msg::Bin(proto::encode(proto::MIC_START, mic).into()));
        }
        st.pages.insert(id, page);
        id
    }

    pub fn leave(&self, id: u64) {
        self.lock().pages.remove(&id);
    }

    pub fn picture(&self) -> Vec<Msg> {
        self.lock().picture()
    }

    /// Whether a request may drive this Kode Dot: browsers send Origin and
    /// ignore CORS for WebSockets, and a form may POST from any site, so
    /// without this any site open in the browser could drive the device,
    /// read it, or install an app in it.
    pub fn origin_ok(&self, origin: Option<&str>) -> bool {
        let Some(o) = origin else { return true };
        let port = self.cfg.http_port;
        o == format!("http://127.0.0.1:{port}") || o == format!("http://localhost:{port}")
    }

    /// A frame from a page.
    pub async fn from_page(self: &Arc<Self>, t: u8, p: &[u8]) {
        let Some((lo, hi)) = from_page_len(t) else { return };
        if p.len() < lo || p.len() > hi {
            return;
        }
        // The board's own inputs, for its chips, decoded here once.
        let input = match t {
            proto::BUTTONS => Some(Input::Buttons(p[0])),
            proto::TOUCH => Some(Input::Touch {
                pressed: p[0] != 0,
                x: u16::from_le_bytes([p[1], p[2]]),
                y: u16::from_le_bytes([p[3], p[4]]),
            }),
            proto::BATTERY => Some(Input::Battery(Battery { percent: p[0].min(100), usb: p[1] != 0 })),
            proto::PCM_IN => Some(Input::Mic(p.to_vec())),
            proto::MOTION => Some(Input::Motion(motion_of(p))),
            _ => None,
        };
        let running = {
            let mut st = self.lock();
            if let Some(Input::Battery(b)) = input {
                st.battery = b;
                st.broadcast_json(json!({"battery": battery_json(&b)}));
            }
            if let Some(Input::Motion(m)) = input {
                st.motion = m;
            }
            st.state == "running"
        };
        if !running {
            // The bottom button is the charger's QON: pressing it powers the board on.
            if matches!(input, Some(Input::Buttons(m)) if m & kodedot_chips::BTN_BOT != 0) {
                self.power_on_from_page().await;
            }
            return;
        }
        if let Some(Input::Buttons(m)) = input {
            self.watch_reset_combo(m);
        }
        let emu = self.lock().emu.clone();
        if let (Some(emu), Some(i)) = (emu, input) {
            emu.input(i);
        }
    }

    /// A boot a page asked for. If esp-emu cannot start, the page hears why
    /// as a crash it can retry, rather than losing its connection.
    async fn power_on_from_page(self: &Arc<Self>) {
        if let Err(e) = self.power_on().await {
            if *self.closing.borrow() {
                return; // given up because kode-emu is stopping: no crash
            }
            eprintln!("kode-emu: {e}");
            self.set_state("crashed", &e);
        }
    }

    fn watch_reset_combo(self: &Arc<Self>, mask: u8) {
        let held = mask & RESET_COMBO == RESET_COMBO;
        let mut st = self.lock();
        if held && st.reset_combo.is_none() {
            let me = self.clone();
            st.reset_combo = Some(tokio::spawn(async move {
                tokio::time::sleep(RESET_HOLD).await;
                me.lock().reset_combo = None;
                me.reset().await;
            }));
        } else if !held {
            if let Some(t) = st.reset_combo.take() {
                t.abort();
            }
        }
    }

    pub async fn reset(&self) {
        let emu = self.lock().running_emu().cloned();
        if let Some(emu) = emu {
            if let Err(e) = emu.reset().await {
                eprintln!("kode-emu: the reset did not reach esp-emu: {e}");
            }
        }
    }

    /// A JSON message from a page.
    pub async fn from_page_json(self: &Arc<Self>, j: &Value) {
        if let Some(sd) = j.get("sd") {
            self.set_card_in(truthy(sd)).await;
        }
        if let Some(on) = j.get("mic") {
            let mut st = self.lock();
            let on = truthy(on) && st.state == "running";
            st.set_mic(on);
        }
        if let Some(c) = j.get("usb").and_then(UsbCmd::from_json) {
            if let Some(u) = self.usb.get() {
                u.command(c);
            }
        }
        if let Some(v) = j.get("console_in") {
            let line = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            self.console_in(&line);
        }
        match j.get("power").and_then(Value::as_str) {
            Some("on") => self.power_on_from_page().await,
            Some("reset") => self.reset().await,
            _ => {}
        }
    }

    /// The card into the slot or out of it. esp-emu takes no card in or out
    /// while it runs, so a Kode Dot that is on restarts with the slot as it
    /// now is, as one would with the card pulled under it.
    async fn set_card_in(self: &Arc<Self>, sd_in: bool) {
        let running = {
            let mut st = self.lock();
            if st.card.is_none() || st.sd_in == sd_in {
                return;
            }
            st.sd_in = sd_in;
            let j = json!({"sd": sd_in});
            st.broadcast_json(j);
            let running = st.running_emu().cloned();
            if running.is_some() {
                st.powering_off = true;
            }
            running
        };
        if let Some(emu) = running {
            self.line(&format!(
                "[kode-emu] the card {}: the Kode Dot restarts with it, as esp-emu cannot change the slot while it runs",
                if sd_in { "goes in" } else { "comes out" }
            ));
            emu.stop().await;
            if let Err(e) = self.power_on().await {
                self.line(&format!("[kode-emu] the Kode Dot did not come back: {e}"));
            }
        }
    }

    /// A line typed at the console (the page's, the terminal's).
    pub fn console_in(&self, line: &str) {
        if let Some(emu) = self.lock().running_emu() {
            emu.console_in(line);
        }
    }

    // ---- the serial port and the card ----------------------------------------

    pub fn open_serial(self: &Arc<Self>) -> std::io::Result<()> {
        let s = Arc::new(Serial::open()?);
        *self.serial.lock().unwrap_or_else(PoisonError::into_inner) = Some(s.clone());
        let flushing = s.clone();
        tokio::spawn(async move { flushing.flush().await });
        let me = self.clone();
        tokio::spawn(async move {
            while let Ok(data) = s.read().await {
                if let Some(emu) = me.lock().running_emu() {
                    emu.console_bytes(&data);
                }
            }
        });
        Ok(())
    }

    pub fn open_card(&self) -> std::io::Result<()> {
        if let Some(path) = &self.cfg.sd {
            self.lock().card = Some(Card::open(path)?);
        }
        Ok(())
    }

    // ---- installs ------------------------------------------------------------

    /// The one install, unless one is under way (the page's upload then
    /// refuses a second): hand it to [`Bridge::install_with`] or
    /// [`Bridge::mailbox`], whose work holds it until it ends.
    pub fn try_installing(&self) -> Option<Installing> {
        self.installing.clone().try_lock_owned().ok()
    }

    /// Is it running, so an app can go in?
    /// How many pages have it open.
    pub fn page_count(&self) -> usize {
        self.lock().pages.len()
    }

    /// Whether any page ever opened it.
    pub fn ever_joined(&self) -> bool {
        self.joined.load(Ordering::SeqCst) > 0
    }

    pub fn is_running(&self) -> bool {
        self.lock().running_emu().is_some()
    }

    /// The state ("off", "running", "crashed") and its detail.
    pub fn state(&self) -> (String, String) {
        let st = self.lock();
        (st.state.clone(), st.detail.clone())
    }

    /// The bytes that have crossed UART1 from the firmware, every boot's.
    pub fn uart_bytes(&self) -> u64 {
        self.uart_bytes.load(Ordering::Relaxed)
    }

    /// The console lines the serial port dropped, nobody reading it.
    pub fn serial_dropped(&self) -> u64 {
        self.serial.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map_or(0, |s| s.dropped())
    }

    /// The emulated time of the Kode Dot running now, in ns since its chip's
    /// last reset: what a review's holds and waits are measured in.
    pub fn sim_time_ns(&self) -> Option<u64> {
        self.lock().running_emu().map(|e| e.now_ns())
    }

    /// The screen as it is now: RGB565, little-endian, W x H.
    pub fn framebuffer(&self) -> Vec<u8> {
        self.lock().fb.clone()
    }

    /// Install `image` in the kodeOS running here, through its console as
    /// kcli does; its progress goes to the console. Waits for an install
    /// under way to end first.
    pub async fn install_image(self: &Arc<Self>, image: Vec<u8>) -> Result<(), InstallError> {
        let one = self.installing.clone().lock_owned().await;
        self.install_with(one, image).await
    }

    /// [`Bridge::install_image`], holding the install `one` already. The
    /// work goes on, and holds `one`, if whoever awaits this goes first.
    pub async fn install_with(self: &Arc<Self>, one: Installing, image: Vec<u8>) -> Result<(), InstallError> {
        let me = self.clone();
        let (mut port, feed) = LinesPort::new(Box::new(move |b: &[u8]| {
            if let Some(emu) = me.lock().emu.clone() {
                emu.console_bytes(b);
            }
        }));
        let tap = Tap::new(self, feed);
        let me = self.clone();
        let done = tokio::task::spawn_blocking(move || {
            let (_one, _tap) = (one, tap); // until the install ends, however it ends
            let mut shown: i64 = -1;
            let mut say = |text: &str| {
                // Progress every 10 %, not every chunk.
                if let Some(rest) = text.strip_prefix("-> ") {
                    let frac = rest.split_whitespace().next().unwrap_or("");
                    if let Some((d, t)) = frac.split_once('/') {
                        let (d, t) = (d.parse::<i64>().unwrap_or(0), t.parse::<i64>().unwrap_or(1).max(1));
                        if d * 10 / t == shown {
                            return;
                        }
                        shown = d * 10 / t;
                    }
                }
                me.line(&format!("[kode-emu] {text}"));
            };
            install::install(&image, &mut port, "the simulator's console", &mut say, install::PING_TIMEOUT)
        });
        done.await.unwrap_or_else(|e| Err(InstallError(format!("the install stopped: {e}"))))
    }

    /// Put `image` in the mailbox and reset the chip, as an Arduino or
    /// esptool upload does on the board: the app goes into app_pending
    /// (found in the partition table at 0x8000) of the flash esp-emu is
    /// running, through its `--control-tcp` (`read-region`,
    /// `write-region`, `reset`), and kodeOS adopts it as it boots. Whatever
    /// is running, no console needed. The flash's offset it went to. The
    /// work goes on, holding the install `one`, if whoever awaits this goes
    /// first: a mailbox written and never reset into is no use.
    pub async fn mailbox(self: &Arc<Self>, one: Installing, image: Vec<u8>) -> Result<u32, String> {
        let emu = self.lock().running_emu().cloned().ok_or("the Kode Dot is off")?;
        let dir = TempDir::new(&format!(
            "kode-emu-mailbox-{}-{}",
            std::process::id(),
            self.next_id.fetch_add(1, Ordering::SeqCst)
        ))?;
        let me = self.clone();
        let work = tokio::spawn(async move {
            let _one = one;
            me.to_mailbox(&emu, &image, &dir.0).await
        });
        work.await.unwrap_or_else(|e| Err(format!("the mailbox upload stopped: {e}")))
    }

    /// One command at a time, each answer checked before the next: the
    /// chip is reset only into a mailbox that took the app.
    async fn to_mailbox(&self, emu: &Emu, image: &[u8], dir: &std::path::Path) -> Result<u32, String> {
        let said = |e: std::io::Error| format!("esp-emu's control port: {e}");
        let table = dir.join("partition-table.bin");
        emu.control(&format!("read-region 0x8000 0xc00 {}", table.display()), Duration::from_secs(10))
            .await
            .map_err(said)?;
        let t = std::fs::read(&table).map_err(|e| format!("the partition table esp-emu read out: {e}"))?;
        let at = install::mailbox_slot(&t, image.len())?;
        let app = dir.join("app.bin");
        std::fs::write(&app, image).map_err(|e| format!("could not write {}: {e}", app.display()))?;
        self.line(&format!("[kode-emu] {} bytes into the mailbox (app_pending, 0x{at:x}); resetting", image.len()));
        emu.control(&format!("write-region 0x{at:x} {}", app.display()), Duration::from_secs(60))
            .await
            .map_err(said)?;
        emu.reset().await.map_err(said)?;
        Ok(at)
    }

    // ---- lifetime ----------------------------------------------------------

    /// Boot: the serial port, the card and the emulator.
    pub async fn start(self: &Arc<Self>) -> Result<(), String> {
        self.open_serial().map_err(|e| format!("could not open a serial port for the console: {e}"))?;
        self.open_card()
            .map_err(|e| format!("could not open the card image {}: {e}", self.cfg.sd.as_ref().unwrap().display()))?;
        self.power_on().await
    }

    /// Boot with no serial port: a Kode Dot nobody reaches but its owner
    /// (a review).
    pub async fn start_headless(self: &Arc<Self>) -> Result<(), String> {
        self.open_card()
            .map_err(|e| format!("could not open the card image {}: {e}", self.cfg.sd.as_ref().unwrap().display()))?;
        self.power_on().await
    }

    /// Stop it for good: a boot under way (a page's) given up and waited
    /// for, then whatever is running stopped.
    pub async fn close(&self) {
        self.closing.send_replace(true);
        let _no_boot = self.powering.lock().await;
        let emu = {
            let mut st = self.lock();
            st.pages.clear();
            st.powering_off = true;
            if let Some(t) = st.reset_combo.take() {
                t.abort();
            }
            st.emu.clone()
        };
        if let Some(emu) = emu {
            emu.stop().await;
        }
        self.serial.lock().unwrap_or_else(PoisonError::into_inner).take();
        self.lock().card = None;
    }
}

/// The one install under way: see [`Bridge::try_installing`].
pub type Installing = tokio::sync::OwnedMutexGuard<()>;

/// An install hearing the console, until it is dropped.
struct Tap {
    bridge: Arc<Bridge>,
    id: u64,
}

impl Tap {
    fn new(bridge: &Arc<Bridge>, feed: std::sync::mpsc::Sender<String>) -> Tap {
        let id = bridge.next_id.fetch_add(1, Ordering::SeqCst);
        bridge.lock().taps.push((id, feed));
        Tap { bridge: bridge.clone(), id }
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.bridge.lock().taps.retain(|(id, _)| *id != self.id);
    }
}

/// A folder of the system's temporary one, gone with whatever is in it once
/// this is dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Result<TempDir, String> {
        let dir = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
        Ok(TempDir(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// JSON's truthiness as Python's bool() has it, for the page's switch.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_c5_with_an_empty_vault_is_joined_to_the_simulator_s_network() {
        assert!(first_join(
            "!N 3741 kaos.radio C5: KODE services v3, 0 networks, 0 BLE keys, identity 0x00; heap 114016 B free"
        ));
        assert!(!first_join("!N 3741 kaos.radio C5: KODE services v3, 1 networks, 0 BLE keys, identity 0x00"));
        assert!(!first_join("[kode-emu] joining kode-emu for you: the C5 knows no network yet"));
    }

    /// Whatever the queue has now, and how many times it said the page
    /// was owed the picture; None if it ended (the page was let go).
    async fn take_all(q: &mut PageQueue) -> Option<(Vec<Msg>, usize)> {
        let mut got = vec![];
        let mut owed = 0;
        loop {
            match tokio::time::timeout(Duration::from_millis(50), q.next()).await {
                Ok(Some((m, caught_up))) => {
                    owed += caught_up as usize;
                    got.push(m);
                }
                Ok(None) => return None,
                Err(_) => return Some((got, owed)),
            }
        }
    }

    fn screen_of(bytes: usize) -> Msg {
        Msg::Bin(proto::encode(proto::FB_RECT, &vec![0; bytes - proto::HEADER]).into())
    }

    #[tokio::test]
    async fn a_page_that_falls_behind_loses_only_screen_and_sound_and_is_owed_the_picture() {
        // Bounded in bytes: whole screens, 411 KB each, as many as fit.
        let screen = screen_of(proto::HEADER + 8 + W * H * 2);
        let fit = MEDIA_BYTES / (proto::HEADER + 8 + W * H * 2);
        let (page, mut q) = Page::new();
        for _ in 0..fit + 10 {
            page.put(screen.clone());
        }
        page.put(Msg::Text("{\"state\":\"off\"}".into())); // never dropped
        page.put(Msg::Bin(proto::encode(proto::LED, &[1, 2, 3]).into()));
        let (got, owed) = take_all(&mut q).await.expect("the page was let go");
        assert_eq!(got.len(), fit + 2);
        assert!(matches!(&got[fit], Msg::Text(t) if t.contains("off")));
        assert_eq!(owed, 1);
    }

    #[tokio::test]
    async fn sound_counts_against_the_same_bytes_as_the_screen() {
        let limits = Limits { media: 10_000, ..Limits::default() };
        let (page, mut q) = Page::with_limits(limits);
        let sound = Msg::Bin(proto::encode(proto::PCM_OUT, &[0; 993]).into()); // 1000 bytes
        for _ in 0..15 {
            page.put(sound.clone());
        }
        let (got, owed) = take_all(&mut q).await.unwrap();
        assert_eq!((got.len(), owed), (10, 1));
    }

    #[tokio::test]
    async fn a_page_whose_console_backlog_passes_its_bound_is_let_go() {
        let limits = Limits { backlog: 10_000, ..Limits::default() };
        let (page, mut q) = Page::with_limits(limits);
        let mut kicked = page.let_go();
        for i in 0..1000 {
            page.put(Msg::Text(format!("{{\"console\": \"line {i}\"}}")));
        }
        assert!(*kicked.borrow_and_update(), "a page 1000 lines behind was kept");
        assert!(take_all(&mut q).await.is_none(), "its queue goes on");
    }

    #[tokio::test]
    async fn a_page_that_takes_nothing_for_too_long_is_let_go() {
        let limits = Limits { stuck: Duration::from_millis(100), ..Limits::default() };
        let (page, mut q) = Page::with_limits(limits);
        let kicked = page.let_go();
        page.put(Msg::Text("a".into()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        page.put(Msg::Text("b".into()));
        assert!(!*kicked.borrow(), "50 ms behind is not stuck");
        let (got, _) = take_all(&mut q).await.unwrap(); // it moves: not stuck
        assert_eq!(got.len(), 2);
        tokio::time::sleep(Duration::from_millis(200)).await; // idle, nothing waiting: fine
        page.put(Msg::Text("c".into()));
        assert!(!*kicked.borrow(), "an idle page is not a stuck one");
        tokio::time::sleep(Duration::from_millis(150)).await; // "c" never taken
        page.put(Msg::Text("d".into()));
        assert!(*kicked.borrow(), "a page that took nothing for 150 ms was kept");
    }

    #[test]
    fn the_motion_frame_is_ten_little_endian_i32s() {
        let mut p = Vec::new();
        for v in [1, -2, 3, 40, -50, 60, 700, -800, 900, 215] {
            p.extend_from_slice(&i32::to_le_bytes(v));
        }
        let m = motion_of(&p);
        assert_eq!((m.accel_mg, m.gyro_mdps, m.mag_mg, m.temp_dc), ([1, -2, 3], [40, -50, 60], [700, -800, 900], 215));
    }

    #[test]
    fn only_the_page_s_own_frames_of_their_own_sizes_pass() {
        assert_eq!(from_page_len(proto::TOUCH), Some((5, 5)));
        assert_eq!(from_page_len(proto::PCM_IN), Some((2, 8185)));
        assert_eq!(from_page_len(proto::MOTION), Some((40, 40)));
        assert_eq!(from_page_len(proto::LED), None);
        assert_eq!(from_page_len(proto::CONSOLE_IN), None);
    }
}

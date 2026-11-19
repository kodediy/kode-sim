//! The computer on the other end of the Kode Dot's USB cable.
//!
//! The firmware's TinyUSB runs unchanged over kemu's device controller
//! (`firmware/kemu/models/usbdcd.c`); this is the host that controller talks
//! to. It plugs and unplugs the cable, enumerates what answers (the device,
//! configuration and string descriptors, SET_ADDRESS, SET_CONFIGURATION), and
//! then behaves as a computer does with what it found:
//!
//! - a **mass storage** device (SD Reader): bulk-only transport and SCSI -
//!   TEST UNIT READY, INQUIRY, READ CAPACITY, MODE SENSE, READ and WRITE (10),
//!   PREVENT/ALLOW and START STOP UNIT (the eject) - and its FAT volume
//!   mounted over those blocks, so that a file can be listed, copied in and
//!   read back through the app's own `tud_msc_*` callbacks and the card;
//! - **HID keyboards and mice**: every active HID interface's Report
//!   descriptor determines its input layouts, usages and Report IDs.
//!   Keyboard usages retain the US and Spanish text translation; mouse
//!   buttons, relative motion and absolute axes remain entirely virtual.
//!
//! The wire is kemu_proto.h's USB_* frames. Everything here runs on one
//! thread of its own, which owns the state and blocks on the frames it
//! waits for; the bridge hands it frames and commands through a channel and
//! takes JSON snapshots back (`{"usb": {...}}`), for the page.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::{hid, proto, usbconfig};

/// USB_BUS kinds.
pub const BUS_CABLE_OUT: u8 = 0;
pub const BUS_CABLE_IN: u8 = 1;
/// Reset the attached bus without moving its cable.
pub const BUS_RESET: u8 = 4;
/// USB_DEV kinds.
pub const DEV_PULLUP: u8 = 1;
pub const DEV_STALL: u8 = 2;
pub const DEV_CLEAR: u8 = 3;
/// The stack opened an endpoint (kemu_usbdcd.h).
pub const DEV_EP_OPEN: u8 = 4;
/// The stack closed an endpoint, or all endpoints (0xff).
pub const DEV_EP_CLOSE: u8 = 6;
/// The firmware's OUT endpoint queue, in bytes, per endpoint (KEMU_USB_CREDIT).
const CREDIT: u32 = 8192;
/// The biggest OUT chunk: kemu's frame buffer is 8 KB.
const CHUNK: usize = 4096;
/// An answer from the firmware that takes longer than this is not coming: the
/// emulated P4 is slow, and a SCSI command may sit in the card for seconds.
const CONTROL_WAIT: Duration = Duration::from_secs(10);
const BULK_WAIT: Duration = Duration::from_secs(30);
/// Sectors one SCSI command moves at most.
const MAX_SECTORS: usize = 128;
/// The lines of the host's log kept for the page.
const LOG_KEPT: usize = 40;
const TYPED_KEPT: usize = 4000;

/// What can go wrong on the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsbErr {
    /// Nothing came in time.
    Timeout(&'static str),
    /// The endpoint stalled (its address).
    Stall(u8),
    /// The device or the cable went away.
    Detached,
    /// A SCSI command failed: the sense key, ASC and ASCQ.
    Scsi(u8, u8, u8),
    /// The device answered something that is not USB or SCSI.
    Protocol(String),
}

impl std::fmt::Display for UsbErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UsbErr::Timeout(w) => write!(f, "no answer ({w})"),
            UsbErr::Stall(ep) => write!(f, "endpoint {ep:#04x} stalled"),
            UsbErr::Detached => write!(f, "the device went away"),
            UsbErr::Scsi(k, a, q) => write!(f, "SCSI {}", sense_text(*k, *a, *q)),
            UsbErr::Protocol(s) => write!(f, "{s}"),
        }
    }
}

/// A SCSI sense, as a person would read it.
pub fn sense_text(key: u8, asc: u8, ascq: u8) -> String {
    let what = match (asc, ascq) {
        (0x3A, _) => "medium not present",
        (0x28, _) => "medium may have changed",
        (0x27, _) => "write protected",
        (0x20, _) => "invalid command",
        (0x24, _) => "invalid field in the command",
        (0x21, _) => "block address out of range",
        (0x11, _) => "unrecovered read error",
        (0x04, _) => "not ready",
        _ => "",
    };
    format!("sense {key:#x}/{asc:#04x}/{ascq:#04x}{}{what}", if what.is_empty() { "" } else { ": " })
}

/// What the page asks the computer to do.
#[derive(Debug, Clone)]
pub enum Cmd {
    /// Plug the cable in (the page also powers the Kode Dot from it).
    Plug,
    /// Pull the cable out, ejected or not.
    Unplug,
    /// Reset and enumerate the device again, keeping the cable plugged in.
    Reset,
    /// Eject the disk as a computer does, and let go of it.
    Eject,
    /// Look at the disk again.
    List,
    /// Copy a file into the disk's root folder.
    Put { name: String, data: Vec<u8> },
    /// Copy a file of `size` pseudo-random bytes in (for the speed).
    PutRandom { name: String, size: usize },
    /// Read a file back: its size and checksum.
    Get { name: String },
    /// The keyboard layout of the computer: "us" or "es".
    Layout(String),
    /// Forget what was typed.
    ClearTyped,
}

impl Cmd {
    /// The page's `{"usb": {"cmd": ...}}`.
    pub fn from_json(j: &Value) -> Option<Cmd> {
        let name = || j.get("name").and_then(Value::as_str).map(clean_name);
        Some(match j.get("cmd")?.as_str()? {
            "plug" => Cmd::Plug,
            "unplug" => Cmd::Unplug,
            "reset" => Cmd::Reset,
            "eject" => Cmd::Eject,
            "list" => Cmd::List,
            "put" => {
                let data = base64_decode(j.get("data")?.as_str()?)?;
                Cmd::Put { name: name()?, data }
            }
            "put_random" => {
                let size = j.get("size")?.as_u64()?.min(64 << 20) as usize;
                Cmd::PutRandom { name: name()?, size }
            }
            "get" => Cmd::Get { name: name()? },
            "layout" => Cmd::Layout(j.get("layout")?.as_str()?.to_string()),
            "clear_typed" => Cmd::ClearTyped,
            _ => return None,
        })
    }
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// A file name that is one name in the root folder: no path, no control bytes.
fn clean_name(n: &str) -> String {
    let n: String = n.chars().filter(|c| !c.is_control() && !"/\\:*?\"<>|".contains(*c)).collect();
    let n = n.trim().trim_matches('.').to_string();
    if n.is_empty() {
        "kode-sim.bin".into()
    } else {
        n.chars().take(100).collect()
    }
}

/// What reaches the computer's thread.
enum Ev {
    /// USB_IN: a transfer the device sent.
    In(u8, Vec<u8>),
    /// USB_DEV.
    Dev(u8, u8),
    /// USB_ACK: the app took `bytes` of what was sent to OUT endpoint `ep`.
    Ack(u8, u32),
    Cmd(Cmd),
    /// The firmware booted (again): the device is gone, the cable is where it was.
    Rebooted,
}

/// Where the computer's frames go: the firmware's UART1, in kode-sim.
pub type Wire = Arc<dyn Fn(u8, &[u8]) + Send + Sync>;
/// Where its snapshots go: every page, and the late ones.
pub type Emit = Arc<dyn Fn(Value) + Send + Sync>;

/// The handle the bridge keeps.
pub struct UsbHost {
    tx: Sender<Ev>,
}

impl UsbHost {
    pub fn start(wire: Wire, emit: Emit) -> UsbHost {
        let (tx, rx) = mpsc::channel();
        let host = Host::new(rx, wire, emit);
        std::thread::Builder::new()
            .name("usb-host".into())
            .spawn(move || host.run())
            .expect("a thread for the USB host");
        UsbHost { tx }
    }

    /// A frame from the firmware: USB_IN, USB_DEV or USB_ACK.
    pub fn firmware_said(&self, t: u8, p: &[u8]) {
        if let Some(ev) = decode(t, p) {
            let _ = self.tx.send(ev);
        }
    }

    pub fn command(&self, c: Cmd) {
        let _ = self.tx.send(Ev::Cmd(c));
    }

    /// The firmware booted: whatever was enumerated is gone.
    pub fn rebooted(&self) {
        let _ = self.tx.send(Ev::Rebooted);
    }
}

fn decode(t: u8, p: &[u8]) -> Option<Ev> {
    Some(match t {
        proto::USB_IN if !p.is_empty() => Ev::In(p[0], p[1..].to_vec()),
        proto::USB_DEV if p.len() >= 4 => Ev::Dev(p[0], p[1]),
        proto::USB_ACK if p.len() >= 5 => Ev::Ack(p[0], u32::from_le_bytes([p[1], p[2], p[3], p[4]])),
        _ => return None,
    })
}

// ---- the computer --------------------------------------------------------

#[derive(Default, Clone)]
struct DeviceInfo {
    vid: u16,
    pid: u16,
    manufacturer: String,
    product: String,
    ep0_mps: usize,
    /// The selected bulk-only MSC interface, for the existing disk driver.
    iface: (u8, u8, u8),
    ep_in: u8,
    ep_out: u8,
    intf: u8,
    configuration: u8,
    interfaces: Vec<usbconfig::Interface>,
    interface_errors: BTreeMap<u8, String>,
}

struct HidInterface {
    descriptor: Option<hid::Descriptor>,
    error: String,
}

/// State belongs to an endpoint, interface, Report ID and application collection.
type HidSource = (u8, u8, Option<u8>, usize);

#[derive(Clone, PartialEq, Eq)]
struct HidState {
    input: hid::InputCollection,
    totals: BTreeMap<u16, i64>,
    presses: BTreeMap<u16, u64>,
    releases: BTreeMap<u16, u64>,
    sequence: u64,
}

#[derive(Default, Clone)]
struct DiskInfo {
    blocks: u64,
    block_size: u32,
    vendor: String,
    product: String,
    write_protected: bool,
    label: String,
    free: u64,
    total: u64,
}

#[derive(Default, Clone)]
struct FileInfo {
    name: String,
    size: u64,
    dir: bool,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Stage {
    #[default]
    NoCable,
    /// Cable in, nothing announced itself.
    Waiting,
    Enumerating,
    Mounted,
    Keyboard,
    Mouse,
    Hid,
    Ejected,
    Failed,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Stage::NoCable => "no_cable",
            Stage::Waiting => "waiting",
            Stage::Enumerating => "enumerating",
            Stage::Mounted => "mounted",
            Stage::Keyboard => "keyboard",
            Stage::Mouse => "mouse",
            Stage::Hid => "hid",
            Stage::Ejected => "ejected",
            Stage::Failed => "failed",
        }
    }
}

struct Host {
    rx: Receiver<Ev>,
    wire: Wire,
    emit: Emit,
    cable: bool,
    pullup: bool,
    /// The firmware restarted and its new boot has not raised its pull-up
    /// yet: whatever comes in until then was the old boot's, answered as it
    /// went (a sector of a read under way), and is not the new one's.
    restarting: bool,
    stage: Stage,
    error: String,
    dev: DeviceInfo,
    disk: DiskInfo,
    files: Vec<FileInfo>,
    /// Bytes the device sent on each IN endpoint, not yet taken.
    ins: HashMap<u8, VecDeque<Vec<u8>>>,
    /// The transfers the device sent on control endpoint 0, one frame each:
    /// where a transfer ends is the answer to "is this all of it".
    ctl: VecDeque<Vec<u8>>,
    /// How many more bytes each OUT endpoint's queue in the firmware takes.
    credit: HashMap<u8, u32>,
    stalled: Vec<u8>,
    cmds: VecDeque<Cmd>,
    log: VecDeque<String>,
    read: u64,
    written: u64,
    /// The last transfer's speed, bytes a second.
    speed: f64,
    last: String,
    layout: String,
    typed: String,
    hid: BTreeMap<u8, HidInterface>,
    hid_states: BTreeMap<HidSource, HidState>,
    pending_hid: VecDeque<(u8, Vec<u8>)>,
    /// A closed endpoint must not decode reports from a previous TinyUSB instance.
    closed_hid_endpoints: BTreeSet<u8>,
    /// A dead key (the Spanish layout's acute or diaeresis) waiting for its letter.
    dead: BTreeMap<(u8, usize), char>,
    tag: u32,
    /// When the last snapshot went out.
    published: Option<Instant>,
    /// Another snapshot is owed: it was thinned.
    owed: bool,
    started: Instant,
}

impl Host {
    fn new(rx: Receiver<Ev>, wire: Wire, emit: Emit) -> Host {
        Host {
            rx,
            wire,
            emit,
            cable: false,
            pullup: false,
            restarting: false,
            stage: Stage::NoCable,
            error: String::new(),
            dev: DeviceInfo::default(),
            disk: DiskInfo::default(),
            files: vec![],
            ins: HashMap::new(),
            ctl: VecDeque::new(),
            credit: HashMap::new(),
            stalled: vec![],
            cmds: VecDeque::new(),
            log: VecDeque::new(),
            read: 0,
            written: 0,
            speed: 0.0,
            last: String::new(),
            layout: "es".into(),
            typed: String::new(),
            hid: BTreeMap::new(),
            hid_states: BTreeMap::new(),
            pending_hid: VecDeque::new(),
            closed_hid_endpoints: BTreeSet::new(),
            dead: BTreeMap::new(),
            tag: 0,
            published: None,
            owed: false,
            started: Instant::now(),
        }
    }

    // ---- the thread ----

    fn run(mut self) {
        self.publish(true);
        loop {
            if let Some(c) = self.cmds.pop_front() {
                self.command(c);
                continue;
            }
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(ev) => self.apply(ev),
                Err(RecvTimeoutError::Timeout) => {
                    if self.owed {
                        self.publish(true);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if self.cable && self.pullup && self.stage == Stage::Waiting {
                self.enumerate();
            }
        }
    }

    /// One thing the firmware or the page did, into the state.
    fn apply(&mut self, ev: Ev) {
        match ev {
            Ev::In(..) | Ev::Ack(..) if self.restarting => {}
            Ev::In(ep, data) => {
                if ep == 0x80 {
                    self.ctl.push_back(data);
                } else if !self.cable || !self.pullup {
                    // No endpoint from the previous attachment is still valid.
                } else if self.closed_hid_endpoints.contains(&ep) {
                    // The DCD closed this endpoint. Wait for its next EP_OPEN event.
                } else if let Some(number) = self.hid_interface(ep) {
                    if self.stage == Stage::Enumerating {
                        let queued: usize = self.pending_hid.iter().map(|(_, d)| d.len().max(1)).sum();
                        if queued + data.len().max(1) <= usize::from(u16::MAX) {
                            self.pending_hid.push_back((ep, data));
                        } else {
                            self.say("HID reports exceed the USB transfer buffer during enumeration".into());
                        }
                    } else {
                        self.report(number, ep, &data);
                    }
                } else if ep == self.dev.ep_in && self.dev.iface.0 == usbconfig::CLASS_MSC {
                    self.ins.entry(ep).or_default().push_back(data);
                }
            }
            Ev::Ack(ep, bytes) => {
                *self.credit.entry(ep).or_insert(CREDIT) += bytes;
            }
            Ev::Dev(DEV_PULLUP, up) => {
                self.pullup = up != 0;
                if self.pullup {
                    self.restarting = false;
                }
                self.say(format!("the device {} its pull-up", if self.pullup { "raises" } else { "drops" }));
                if !self.pullup {
                    self.gone("the device let go of the bus");
                } else if self.cable && matches!(self.stage, Stage::NoCable | Stage::Ejected) {
                    self.stage = Stage::Waiting;
                }
            }
            Ev::Dev(DEV_STALL, ep) => {
                if !self.stalled.contains(&ep) {
                    self.stalled.push(ep);
                }
            }
            Ev::Dev(DEV_CLEAR, ep) => self.stalled.retain(|e| *e != ep),
            Ev::Dev(DEV_EP_OPEN, ep) => {
                self.closed_hid_endpoints.remove(&ep);
            }
            Ev::Dev(DEV_EP_CLOSE, ep) => {
                if ep == 0xff {
                    self.closed_hid_endpoints.extend(
                        self.dev
                            .interfaces
                            .iter()
                            .filter(|i| i.alternate == 0 && i.class.0 == usbconfig::CLASS_HID)
                            .flat_map(|i| i.endpoints.iter().filter(|e| e.input()).map(|e| e.address)),
                    );
                } else {
                    self.closed_hid_endpoints.insert(ep);
                }
                self.hid_states.retain(|(_, source_ep, _, _), _| ep != 0xff && *source_ep != ep);
                self.dead.retain(|(interface, collection), _| {
                    self.hid_states.keys().any(|(i, _, _, c)| i == interface && c == collection)
                });
                self.pending_hid.retain(|(source_ep, _)| ep != 0xff && *source_ep != ep);
                self.ins.retain(|source_ep, _| ep != 0xff && *source_ep != ep);
                self.publish(true);
            }
            Ev::Dev(..) => {}
            Ev::Cmd(c) => self.cmds.push_back(c),
            Ev::Rebooted => {
                self.pullup = false;
                self.restarting = true;
                self.gone("the Kode Dot restarted");
                if self.cable {
                    // The cable did not move: the new boot finds it in.
                    (self.wire)(proto::USB_BUS, &[BUS_CABLE_IN]);
                    self.stage = Stage::Waiting;
                }
            }
        }
    }

    /// Nothing is mounted any more.
    fn gone(&mut self, why: &str) {
        // After an eject the device leaves the bus: that is the eject being done,
        // and the page keeps saying so until the device comes back.
        let ejected = self.stage == Stage::Ejected && self.cable;
        if matches!(
            self.stage,
            Stage::Enumerating | Stage::Mounted | Stage::Keyboard | Stage::Mouse | Stage::Hid | Stage::Failed
        ) {
            self.say(format!("{why}: the computer lets go of the device"));
        }
        self.stage = if ejected {
            Stage::Ejected
        } else if self.cable {
            Stage::Waiting
        } else {
            Stage::NoCable
        };
        self.dev = DeviceInfo::default();
        self.disk = DiskInfo::default();
        self.files.clear();
        self.ins.clear();
        self.ctl.clear();
        self.credit.clear();
        self.stalled.clear();
        self.hid.clear();
        self.hid_states.clear();
        self.pending_hid.clear();
        self.closed_hid_endpoints.clear();
        self.dead.clear();
        self.error.clear();
        self.publish(true);
    }

    fn command(&mut self, c: Cmd) {
        match c {
            Cmd::Plug => {
                if !self.cable {
                    self.cable = true;
                    self.say("cable plugged in".into());
                    (self.wire)(proto::USB_BUS, &[BUS_CABLE_IN]);
                    self.stage = Stage::Waiting;
                    self.publish(true);
                }
            }
            Cmd::Unplug => {
                if self.cable {
                    self.cable = false;
                    (self.wire)(proto::USB_BUS, &[BUS_CABLE_OUT]);
                    let ejected = self.stage == Stage::Ejected;
                    self.say(if ejected {
                        "cable pulled out (after ejecting)".into()
                    } else {
                        "cable pulled out".into()
                    });
                    self.stage = Stage::NoCable;
                    self.dev = DeviceInfo::default();
                    self.disk = DiskInfo::default();
                    self.files.clear();
                    self.ins.clear();
                    self.ctl.clear();
                    self.credit.clear();
                    self.hid.clear();
                    self.hid_states.clear();
                    self.pending_hid.clear();
                    self.closed_hid_endpoints.clear();
                    self.dead.clear();
                    self.error.clear();
                    self.publish(true);
                }
            }
            Cmd::Reset => {
                if self.cable && self.pullup {
                    self.gone("the computer resets the bus");
                    self.stage = Stage::Waiting;
                    self.publish(true);
                    (self.wire)(proto::USB_BUS, &[BUS_RESET]);
                }
            }
            Cmd::Layout(l) => {
                self.layout = if l == "us" { "us".into() } else { "es".into() };
                self.publish(true);
            }
            Cmd::ClearTyped => {
                self.typed.clear();
                self.publish(true);
            }
            other if self.stage == Stage::Mounted => {
                let r = self.disk_command(other);
                if let Err(e) = r {
                    self.say(format!("failed: {e}"));
                    self.last = format!("failed: {e}");
                    if matches!(e, UsbErr::Detached) {
                        self.gone("the device went away");
                    }
                }
                self.publish(true);
            }
            _ => {
                self.say("there is no disk mounted".into());
                self.publish(true);
            }
        }
    }

    // ---- the page's snapshot ----

    fn say(&mut self, line: String) {
        let t = self.started.elapsed().as_secs_f64();
        self.log.push_back(format!("{t:7.2}s  {line}"));
        while self.log.len() > LOG_KEPT {
            self.log.pop_front();
        }
        self.owed = true;
    }

    fn snapshot(&self) -> Value {
        let (keyboard, mouse) = self.capabilities();
        let mut keys = BTreeSet::new();
        let mut modifiers = 0;
        let mut buttons = BTreeSet::new();
        for state in self.hid_states.values() {
            if let Some(k) = &state.input.keyboard {
                keys.extend(k.keys.iter().copied());
                modifiers |= k.modifiers;
            }
            if let Some(m) = &state.input.mouse {
                buttons.extend(m.buttons.iter().copied());
            }
        }
        let mut j = json!({
            "cable": self.cable,
            "stage": self.stage.name(),
            "error": self.error,
            "log": self.log,
            "read": self.read,
            "written": self.written,
            "speed": self.speed,
            "last": self.last,
            "layout": self.layout,
            "typed": self.typed,
            "keys_down": keys,
            "modifiers": modifiers,
            "mouse": {"buttons": buttons},
            "configuration": self.dev.configuration,
            "interfaces": self.dev.interfaces.iter().map(|i| {
                let driver = (i.alternate == 0).then(|| self.hid.get(&i.number)).flatten();
                let descriptor = driver.and_then(|h| h.descriptor.as_ref());
                json!({
                    "number": i.number, "alternate": i.alternate, "active": i.alternate == 0,
                    "class": i.class.0, "subclass": i.class.1, "protocol": i.class.2,
                    "endpoints": i.endpoints.iter().map(|e| json!({
                        "address": e.address, "direction": if e.input() {"in"} else {"out"},
                        "transfer_type": e.transfer(), "attributes": e.attributes,
                        "max_packet_size": e.max_packet_size, "transactions_per_microframe": e.transactions, "interval": e.interval,
                    })).collect::<Vec<_>>(),
                    "hid_descriptor": i.hid_descriptor, "report_length": i.report_length,
                    "keyboard": descriptor.is_some_and(hid::Descriptor::keyboard),
                    "mouse": descriptor.is_some_and(hid::Descriptor::mouse),
                    "buttons": descriptor.map(hid::Descriptor::buttons).unwrap_or_default(),
                    "hid_protocol": descriptor.map(|_| "report"),
                    "error": driver.map(|h| h.error.as_str()).or_else(|| self.dev.interface_errors.get(&i.number).map(String::as_str)).unwrap_or(""),
                    "diagnostics": descriptor.map(|d| d.diagnostics.as_slice()).unwrap_or(&[]),
                    "reports": descriptor.map(|d| d.report_lengths(hid::ReportKind::Input)).unwrap_or_default()
                        .iter().map(|(id, length)| json!({"report_id":id,"length":length})).collect::<Vec<_>>(),
                    "collections": descriptor.map(|d| d.collections.iter().enumerate().map(|(id, c)| json!({
                        "id": id, "parent": c.parent, "type": c.kind,
                        "usage_page": c.usage.map(|u| u.page), "usage": c.usage.map(|u| u.id),
                        "keyboard": c.keyboard(), "mouse": c.mouse(),
                    })).collect::<Vec<_>>()).unwrap_or_default(),
                })
            }).collect::<Vec<_>>(),
            "hid": {
                "keyboard": keyboard, "mouse": mouse,
                "reports": self.hid_states.iter().map(|((interface, endpoint, report_id, collection), state)| {
                    json!({
                        "interface": interface, "endpoint": endpoint, "report_id": report_id, "collection": collection,
                        "keys_down": state.input.keyboard.as_ref().map(|k| &k.keys),
                        "modifiers": state.input.keyboard.as_ref().map(|k| k.modifiers),
                        "mouse": state.input.mouse.as_ref().map(|m| json!({
                            "buttons": m.buttons, "relative": axes_json(&m.relative), "absolute": axes_json(&m.absolute),
                            "total": axes_json(&state.totals), "sequence": state.sequence,
                            "presses": state.presses, "releases": state.releases,
                        })),
                    })
                }).collect::<Vec<_>>(),
            },
        });
        if self.dev.vid != 0 || self.dev.pid != 0 {
            j["device"] = json!({
                "vid": format!("{:04x}", self.dev.vid), "pid": format!("{:04x}", self.dev.pid),
                "manufacturer": self.dev.manufacturer, "product": self.dev.product,
                "class": self.device_class(),
            });
        }
        if self.disk.blocks != 0 {
            j["disk"] = json!({
                "blocks": self.disk.blocks, "block_size": self.disk.block_size,
                "vendor": self.disk.vendor, "product": self.disk.product,
                "write_protected": self.disk.write_protected, "label": self.disk.label,
                "free": self.disk.free, "total": self.disk.total,
                "files": self.files.iter().map(|f| json!({"name": f.name, "size": f.size, "dir": f.dir})).collect::<Vec<_>>(),
            });
        }
        json!({ "usb": j })
    }

    fn capabilities(&self) -> (bool, bool) {
        let descriptors = || self.hid.values().filter_map(|h| h.descriptor.as_ref());
        (descriptors().any(hid::Descriptor::keyboard), descriptors().any(hid::Descriptor::mouse))
    }

    fn device_class(&self) -> &'static str {
        if self.dev.interfaces.iter().filter(|i| i.alternate == 0).count() > 1 || self.capabilities() == (true, true) {
            "composite"
        } else {
            match self.capabilities() {
                (true, false) => "keyboard",
                (false, true) => "mouse",
                _ => class_name(self.dev.iface),
            }
        }
    }

    fn hid_interface(&self, endpoint: u8) -> Option<u8> {
        self.dev
            .interfaces
            .iter()
            .find(|i| {
                i.alternate == 0
                    && i.class.0 == usbconfig::CLASS_HID
                    && i.endpoints
                        .iter()
                        .any(|e| e.address == endpoint && e.input() && e.transfer() == usbconfig::TRANSFER_INTERRUPT)
            })
            .map(|i| i.number)
    }

    /// A snapshot to every page; thinned to five a second unless `now`.
    fn publish(&mut self, now: bool) {
        if !now && self.published.is_some_and(|t| t.elapsed() < Duration::from_millis(200)) {
            self.owed = true;
            return;
        }
        self.published = Some(Instant::now());
        self.owed = false;
        (self.emit)(self.snapshot());
    }

    // ---- waiting for the firmware ----

    /// Take one event, waiting at most until `deadline`; commands wait their turn.
    fn pump(&mut self, deadline: Instant) -> Result<(), UsbErr> {
        let left = deadline.saturating_duration_since(Instant::now());
        match self.rx.recv_timeout(left) {
            Ok(ev) => {
                let was_up = self.pullup && self.cable;
                self.apply(ev);
                if was_up && !(self.pullup && self.cable) {
                    return Err(UsbErr::Detached);
                }
                Ok(())
            }
            Err(RecvTimeoutError::Timeout) => Err(UsbErr::Timeout("the firmware")),
            Err(RecvTimeoutError::Disconnected) => Err(UsbErr::Detached),
        }
    }

    fn attached(&self) -> Result<(), UsbErr> {
        if self.cable && self.pullup {
            Ok(())
        } else {
            Err(UsbErr::Detached)
        }
    }

    /// A control read: the IN data stage, then the status stage (an OUT of
    /// nothing, which the stack's control driver waits for).
    fn control(&mut self, setup: [u8; 8], data: &[u8]) -> Result<Vec<u8>, UsbErr> {
        self.attached()?;
        let w_length = u16::from_le_bytes([setup[6], setup[7]]) as usize;
        let mps = if self.dev.ep0_mps == 0 { 64 } else { self.dev.ep0_mps };
        self.ctl.clear();
        self.stalled.retain(|e| *e & 0x7f != 0);
        (self.wire)(proto::USB_SETUP, &setup);
        if setup[0] & 0x80 == 0 {
            if w_length > 0 {
                let mut frame = vec![0u8];
                frame.extend_from_slice(data);
                (self.wire)(proto::USB_OUT, &frame);
            }
            return self.control_status(true).map(|()| Vec::new());
        }
        let deadline = Instant::now() + CONTROL_WAIT;
        let mut got = Vec::new();
        loop {
            if self.stalled.iter().any(|e| *e & 0x7f == 0) {
                self.stalled.retain(|e| *e & 0x7f != 0);
                return Err(UsbErr::Stall(0x80));
            }
            let mut done = false;
            while let Some(f) = self.ctl.pop_front() {
                if f.len() > mps || f.len() > w_length.saturating_sub(got.len()) {
                    return Err(UsbErr::Protocol("control data exceeds its packet size or requested length".into()));
                }
                done = f.len() < mps;
                got.extend_from_slice(&f);
                if done || got.len() >= w_length {
                    done = true;
                    break;
                }
            }
            if done {
                (self.wire)(proto::USB_OUT, &[0]);
                return Ok(got);
            }
            self.pump(deadline).map_err(|e| {
                if matches!(e, UsbErr::Timeout(_)) {
                    UsbErr::Timeout("a control transfer")
                } else {
                    e
                }
            })?;
        }
    }

    /// A control write: the OUT data stage when there is one, then the
    /// status stage, an IN of nothing.
    fn control_write(&mut self, setup: [u8; 8], data: &[u8]) -> Result<(), UsbErr> {
        self.attached()?;
        self.ctl.clear();
        self.stalled.retain(|e| *e & 0x7f != 0);
        (self.wire)(proto::USB_SETUP, &setup);
        if !data.is_empty() {
            let mut frame = vec![0u8];
            frame.extend_from_slice(data);
            (self.wire)(proto::USB_OUT, &frame);
        }
        self.control_status(false)
    }

    /// The status stage of a write: wait for the empty IN. (`_` keeps the
    /// two callers' shape the same.)
    fn control_status(&mut self, _write: bool) -> Result<(), UsbErr> {
        let deadline = Instant::now() + CONTROL_WAIT;
        loop {
            if self.stalled.iter().any(|e| *e & 0x7f == 0) {
                self.stalled.retain(|e| *e & 0x7f != 0);
                return Err(UsbErr::Stall(0));
            }
            if let Some(status) = self.ctl.pop_front() {
                return if status.is_empty() {
                    Ok(())
                } else {
                    Err(UsbErr::Protocol("nonempty USB control status stage".into()))
                };
            }
            self.pump(deadline).map_err(|e| {
                if matches!(e, UsbErr::Timeout(_)) {
                    UsbErr::Timeout("a control write's status")
                } else {
                    e
                }
            })?;
        }
    }

    /// The data stage of a bulk-in command: what the device sends on `ep`
    /// until it has sent `n` bytes, or - a device may send less than it was
    /// asked for - until it sends the status of command `tag`.
    fn bulk_data(&mut self, ep: u8, n: usize, tag: u32) -> Result<Vec<u8>, UsbErr> {
        let deadline = Instant::now() + BULK_WAIT;
        let mut got = Vec::with_capacity(n);
        while got.len() < n {
            if let Some(f) = self.ins.get_mut(&ep).and_then(|q| q.front()) {
                if is_csw(f, tag) {
                    break;
                }
                let f = self.ins.get_mut(&ep).and_then(VecDeque::pop_front).unwrap_or_default();
                got.extend_from_slice(&f);
                continue;
            }
            if self.stalled.contains(&ep) {
                return Err(UsbErr::Stall(ep));
            }
            self.pump(deadline).map_err(|e| {
                if matches!(e, UsbErr::Timeout(_)) {
                    UsbErr::Timeout("a bulk read")
                } else {
                    e
                }
            })?;
        }
        got.truncate(n);
        Ok(got)
    }

    /// The 13-byte status of a bulk-only command.
    fn bulk_csw(&mut self, ep: u8) -> Result<Vec<u8>, UsbErr> {
        let deadline = Instant::now() + BULK_WAIT;
        loop {
            if let Some(f) = self.ins.get_mut(&ep).and_then(VecDeque::pop_front) {
                return Ok(f);
            }
            if self.stalled.contains(&ep) {
                return Err(UsbErr::Stall(ep));
            }
            self.pump(deadline).map_err(|e| {
                if matches!(e, UsbErr::Timeout(_)) {
                    UsbErr::Timeout("a command status")
                } else {
                    e
                }
            })?;
        }
    }

    /// `data` to OUT endpoint `ep`, as fast as the app takes it.
    fn bulk_out(&mut self, ep: u8, data: &[u8]) -> Result<(), UsbErr> {
        let deadline = Instant::now() + BULK_WAIT;
        for chunk in data.chunks(CHUNK) {
            while self.credit.get(&ep).copied().unwrap_or(CREDIT) < chunk.len() as u32 {
                if self.stalled.contains(&ep) {
                    return Err(UsbErr::Stall(ep));
                }
                self.pump(deadline).map_err(|e| {
                    if matches!(e, UsbErr::Timeout(_)) {
                        UsbErr::Timeout("a bulk write")
                    } else {
                        e
                    }
                })?;
            }
            self.attached()?;
            *self.credit.entry(ep).or_insert(CREDIT) -= chunk.len() as u32;
            let mut frame = Vec::with_capacity(1 + chunk.len());
            frame.push(ep & 0x7f);
            frame.extend_from_slice(chunk);
            (self.wire)(proto::USB_OUT, &frame);
        }
        Ok(())
    }

    // ---- enumeration ----

    fn get_descriptor(&mut self, kind: u8, index: u8, lang: u16, len: u16) -> Result<Vec<u8>, UsbErr> {
        let s = [0x80, 6, index, kind, lang as u8, (lang >> 8) as u8, len as u8, (len >> 8) as u8];
        self.control(s, &[])
    }

    fn get_string(&mut self, index: u8) -> String {
        if index == 0 {
            return String::new();
        }
        match self.get_descriptor(3, index, 0x0409, 126) {
            Ok(d) if d.len() >= 2 => {
                let units: Vec<u16> = d[2..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
                String::from_utf16_lossy(&units)
            }
            _ => String::new(),
        }
    }

    fn enumerate(&mut self) {
        self.stage = Stage::Enumerating;
        self.error.clear();
        self.say("the device announced itself: the computer resets the bus and enumerates it".into());
        self.publish(true);
        // A bus reset takes the device a moment to settle on.
        std::thread::sleep(Duration::from_millis(100));
        match self.enumerate_inner() {
            Ok(()) => {}
            Err(UsbErr::Detached) => self.gone("the device went away while it was enumerated"),
            Err(e) => {
                self.say(format!("enumeration failed: {e}"));
                self.error = e.to_string();
                self.stage = Stage::Failed;
            }
        }
        self.publish(true);
    }

    fn enumerate_inner(&mut self) -> Result<(), UsbErr> {
        let head = self.get_descriptor(1, 0, 0, 8)?;
        if head.len() != 8 || head[0] != 18 || head[1] != 1 || !matches!(head[7], 8 | 16 | 32 | 64) {
            return Err(UsbErr::Protocol("invalid device descriptor or endpoint-zero packet size".into()));
        }
        self.dev.ep0_mps = usize::from(head[7]);
        let d = self.get_descriptor(1, 0, 0, 18)?;
        if d.len() != 18 || d[..8] != head || d[17] == 0 {
            return Err(UsbErr::Protocol("invalid or inconsistent USB device descriptor".into()));
        }
        self.dev = DeviceInfo {
            vid: u16::from_le_bytes([d[8], d[9]]),
            pid: u16::from_le_bytes([d[10], d[11]]),
            ep0_mps: usize::from(d[7]),
            ..DeviceInfo::default()
        };
        self.say(format!("GET_DESCRIPTOR device: {:04x}:{:04x}, class {:#04x}", self.dev.vid, self.dev.pid, d[4]));
        self.control_write([0x00, 5, 1, 0, 0, 0, 0, 0], &[])?;
        self.say("SET_ADDRESS 1".into());
        self.dev.manufacturer = self.get_string(d[14]);
        self.dev.product = self.get_string(d[15]);
        self.say(format!("strings: {:?} {:?}", self.dev.manufacturer, self.dev.product));
        let head = self.get_descriptor(2, 0, 0, 9)?;
        if head.len() != 9 || head[0] != 9 || head[1] != 2 {
            return Err(UsbErr::Protocol("invalid configuration descriptor header".into()));
        }
        let total = u16::from_le_bytes([head[2], head[3]]);
        if total < 9 {
            return Err(UsbErr::Protocol("invalid wTotalLength".into()));
        }
        let cfg = self.get_descriptor(2, 0, 0, total)?;
        if cfg.get(..9) != Some(head.as_slice()) {
            return Err(UsbErr::Protocol("configuration header changed during enumeration".into()));
        }
        self.parse_config(&cfg)?;
        self.control_write([0x00, 9, self.dev.configuration, 0, 0, 0, 0, 0], &[])?;
        self.say(format!(
            "SET_CONFIGURATION {}: interfaces {:?}, endpoints {:?}",
            self.dev.configuration,
            self.dev.interfaces.iter().map(|i| (i.number, i.alternate, i.class)).collect::<Vec<_>>(),
            self.dev
                .interfaces
                .iter()
                .filter(|i| i.alternate == 0)
                .flat_map(|i| i.endpoints.iter())
                .map(|e| format!("{:#04x}", e.address))
                .collect::<Vec<_>>()
        ));
        let mut interfaces: Vec<_> = self.dev.interfaces.iter().filter(|i| i.alternate == 0).cloned().collect();
        interfaces.sort_by_key(|i| i.number);
        for interface in &interfaces {
            if interface.class.0 != usbconfig::CLASS_HID {
                continue;
            }
            let result = self.init_hid(interface);
            match result {
                Ok(descriptor) => {
                    for diagnostic in &descriptor.diagnostics {
                        self.say(format!("HID interface {}: {diagnostic}", interface.number));
                    }
                    self.hid
                        .insert(interface.number, HidInterface { descriptor: Some(descriptor), error: String::new() });
                }
                Err(UsbErr::Detached) => return Err(UsbErr::Detached),
                Err(e) => {
                    self.say(format!("HID interface {} unavailable: {e}", interface.number));
                    self.hid.insert(interface.number, HidInterface { descriptor: None, error: e.to_string() });
                }
            }
        }
        let capabilities = self.capabilities();
        self.stage = match capabilities {
            (true, true) => Stage::Hid,
            (true, false) => Stage::Keyboard,
            (false, true) => Stage::Mouse,
            _ => Stage::Mounted,
        };
        let pending = std::mem::take(&mut self.pending_hid);
        for (ep, data) in pending {
            if let Some(number) = self.hid_interface(ep) {
                self.report(number, ep, &data);
            }
        }
        if self.dev.iface.0 == usbconfig::CLASS_MSC && self.dev.ep_in != 0 && self.dev.ep_out != 0 {
            if let Err(e) = self.mount() {
                if capabilities == (false, false) || e == UsbErr::Detached {
                    return Err(e);
                }
                self.error = format!("MSC interface {}: {e}", self.dev.intf);
                self.say(self.error.clone());
            }
        } else if capabilities == (false, false) && !self.hid.is_empty() {
            return Err(UsbErr::Protocol("no supported HID keyboard or mouse input: see interface diagnostics".into()));
        } else if capabilities == (false, false) {
            self.say("no driver for the active USB interfaces; the device stays plugged in".into());
        }
        Ok(())
    }

    fn parse_config(&mut self, bytes: &[u8]) -> Result<(), UsbErr> {
        let configuration = usbconfig::Configuration::parse(bytes).map_err(UsbErr::Protocol)?;
        self.dev.configuration = configuration.value;
        self.dev.interfaces = configuration.interfaces;
        let mut storage: Vec<_> = self
            .dev
            .interfaces
            .iter()
            .filter(|i| i.alternate == 0 && i.class == (usbconfig::CLASS_MSC, 6, 0x50))
            .cloned()
            .collect();
        storage.sort_by_key(|i| i.number);
        for interface in self.dev.interfaces.iter().filter(|i| {
            i.alternate == 0 && i.class.0 != usbconfig::CLASS_HID && i.class != (usbconfig::CLASS_MSC, 6, 0x50)
        }) {
            self.dev
                .interface_errors
                .insert(interface.number, format!("unsupported USB interface class {:?}", interface.class));
        }
        for interface in storage {
            if self.dev.ep_in != 0 {
                self.dev.interface_errors.insert(interface.number, "additional MSC volume is not mounted".into());
                continue;
            }
            if interface.endpoints.iter().any(|e| !e.representable()) {
                self.dev
                    .interface_errors
                    .insert(interface.number, "MSC endpoint cannot be represented by kemu's controller".into());
                continue;
            }
            let endpoints = |input| {
                interface
                    .endpoints
                    .iter()
                    .filter(move |e| e.input() == input && e.transfer() == usbconfig::TRANSFER_BULK)
            };
            let input: Vec<_> = endpoints(true).collect();
            let output: Vec<_> = endpoints(false).collect();
            if input.len() != 1 || output.len() != 1 {
                self.dev
                    .interface_errors
                    .insert(interface.number, "bulk-only MSC requires one bulk IN and one bulk OUT endpoint".into());
                continue;
            }
            self.dev.intf = interface.number;
            self.dev.iface = interface.class;
            self.dev.ep_in = input[0].address;
            self.dev.ep_out = output[0].address;
        }
        if self.dev.ep_in == 0 {
            if let Some(interface) = self.dev.interfaces.iter().filter(|i| i.alternate == 0).min_by_key(|i| i.number) {
                self.dev.iface = interface.class;
            }
        }
        Ok(())
    }

    // ---- descriptor-driven HID ----

    fn init_hid(&mut self, interface: &usbconfig::Interface) -> Result<hid::Descriptor, UsbErr> {
        if interface.endpoints.iter().any(|e| !e.representable()) {
            return Err(UsbErr::Protocol("HID endpoint cannot be represented by kemu's controller".into()));
        }
        if !interface.endpoints.iter().any(|e| e.input() && e.transfer() == usbconfig::TRANSFER_INTERRUPT) {
            return Err(UsbErr::Protocol("HID interface has no interrupt IN endpoint".into()));
        }
        let length =
            interface.report_length.ok_or_else(|| UsbErr::Protocol("HID interface has no Report descriptor".into()))?;
        let intf = interface.number;
        let r = self.control([0x81, 6, 0, usbconfig::DESC_REPORT, intf, 0, length as u8, (length >> 8) as u8], &[])?;
        if r.len() != usize::from(length) {
            return Err(UsbErr::Protocol(format!("HID interface {intf}: Report descriptor length mismatch")));
        }
        let descriptor =
            hid::Descriptor::parse(&r).map_err(|e| UsbErr::Protocol(format!("HID interface {intf}: {e}")))?;
        self.say(format!("GET_DESCRIPTOR HID report: interface {intf}, {} bytes", r.len()));
        if interface.class.1 == 1 {
            // A boot-capable interface still uses its declared Report layouts.
            self.control_write([0x21, 0x0b, 1, 0, intf, 0, 0, 0], &[]).map_err(|e| {
                if e == UsbErr::Detached {
                    e
                } else {
                    UsbErr::Protocol(format!("SET_PROTOCOL Report failed on interface {intf}: {e}"))
                }
            })?;
            self.say(format!("SET_PROTOCOL Report: interface {intf}"));
        }
        match self.control_write([0x21, 0x0a, 0, 0, intf, 0, 0, 0], &[]) {
            Ok(()) | Err(UsbErr::Stall(_)) => {}
            Err(e) => return Err(e),
        }
        Ok(descriptor)
    }

    fn report(&mut self, interface: u8, endpoint: u8, report: &[u8]) {
        let Some(driver) = self.hid.get(&interface) else {
            return;
        };
        let Some(descriptor) = &driver.descriptor else {
            return;
        };
        let decoded = descriptor.input(report).and_then(|input| {
            let mut updates = Vec::new();
            for collection in input.collections {
                let source = (interface, endpoint, input.report_id, collection.collection);
                let previous = self.hid_states.get(&source);
                let mut state = HidState {
                    input: collection,
                    totals: previous.map(|s| s.totals.clone()).unwrap_or_default(),
                    presses: previous.map(|s| s.presses.clone()).unwrap_or_default(),
                    releases: previous.map(|s| s.releases.clone()).unwrap_or_default(),
                    sequence: previous.map_or(0, |s| s.sequence),
                };
                if let Some(mouse) = &state.input.mouse {
                    let previous_mouse = previous.and_then(|s| s.input.mouse.as_ref());
                    let event = mouse.relative.values().any(|delta| *delta != 0) || previous_mouse != Some(mouse);
                    if event {
                        state.sequence = state
                            .sequence
                            .checked_add(1)
                            .ok_or_else(|| hid::Error("mouse event sequence overflow".into()))?;
                    }
                    for (axis, delta) in &mouse.relative {
                        let total = state.totals.entry(*axis).or_default();
                        *total = total
                            .checked_add(*delta)
                            .ok_or_else(|| hid::Error("mouse motion total overflow".into()))?;
                    }
                    for button in &mouse.buttons {
                        if !previous_mouse.is_some_and(|m| m.buttons.contains(button)) {
                            count_button(&mut state.presses, *button)?;
                        }
                    }
                    if let Some(previous_mouse) = previous_mouse {
                        for button in previous_mouse.buttons.difference(&mouse.buttons) {
                            count_button(&mut state.releases, *button)?;
                        }
                    }
                }
                updates.push((source, state));
            }
            Ok(updates)
        });
        match decoded {
            Err(e) => {
                let error = format!("interface {interface}, endpoint {endpoint:#04x}: {e}");
                if self.hid.get(&interface).is_some_and(|h| h.error != error) {
                    self.hid.get_mut(&interface).expect("known interface").error = error.clone();
                    self.say(error);
                    self.publish(false);
                }
            }
            Ok(updates) => {
                let mut changed = self.hid.get(&interface).is_some_and(|h| !h.error.is_empty());
                if let Some(h) = self.hid.get_mut(&interface) {
                    h.error.clear();
                }
                // Install every collection only after the entire report validates.
                let old: Vec<_> =
                    updates.iter().map(|(source, _)| (*source, self.hid_states.get(source).cloned())).collect();
                for (source, state) in &updates {
                    changed |= self.hid_states.get(source) != Some(state);
                    self.hid_states.insert(*source, state.clone());
                }
                for ((source, state), (_, previous)) in updates.iter().zip(old) {
                    if let Some(keyboard) = &state.input.keyboard {
                        let modifiers = self
                            .hid_states
                            .iter()
                            .filter(|((i, _, _, c), _)| *i == interface && *c == source.3)
                            .filter_map(|(_, s)| s.input.keyboard.as_ref())
                            .fold(0, |m, k| m | k.modifiers);
                        let previous_keys = previous.as_ref().and_then(|s| s.input.keyboard.as_ref());
                        for key in &keyboard.keys {
                            if !previous_keys.is_some_and(|k| k.keys.contains(key)) {
                                self.type_key((interface, source.3), modifiers, *key);
                            }
                        }
                    }
                }
                if changed {
                    self.publish(false);
                }
            }
        }
    }

    fn type_key(&mut self, source: (u8, usize), modifiers: u8, usage: u16) {
        // Translation covers the existing layouts; wider usages remain observable.
        let Ok(usage) = u8::try_from(usage) else {
            return;
        };
        let key = key_char(&self.layout, modifiers, usage);
        if let (Key::Char(c @ ('\u{b4}' | '\u{a8}')), "es") = (&key, self.layout.as_str()) {
            if let Some(previous) = self.dead.insert(source, *c) {
                self.typed.push(previous);
            }
            self.trim_typed();
            return;
        }
        match key {
            Key::Char(c) => {
                if let Some(dead) = self.dead.remove(&source) {
                    match accent(dead, c) {
                        Some(a) => self.typed.push(a),
                        None => {
                            self.typed.push(dead);
                            self.typed.push(c);
                        }
                    }
                } else {
                    self.typed.push(c);
                }
            }
            Key::Backspace => {
                self.typed.pop();
            }
            Key::None => {}
        }
        self.trim_typed();
    }

    fn trim_typed(&mut self) {
        while self.typed.len() > TYPED_KEPT {
            let cut = self.typed.char_indices().nth(1).map_or(self.typed.len(), |(i, _)| i);
            self.typed.drain(..cut);
        }
    }

    // ---- a disk ----

    fn bot(&mut self, cb: &[u8], len: usize, dir_in: bool, out: &[u8]) -> Result<Vec<u8>, UsbErr> {
        let (ep_in, ep_out) = (self.dev.ep_in, self.dev.ep_out);
        self.tag = self.tag.wrapping_add(1);
        let mut cbw = Vec::with_capacity(31);
        cbw.extend_from_slice(b"USBC");
        cbw.extend_from_slice(&self.tag.to_le_bytes());
        cbw.extend_from_slice(&(len as u32).to_le_bytes());
        cbw.push(if dir_in { 0x80 } else { 0 });
        cbw.push(0);
        cbw.push(cb.len() as u8);
        cbw.extend_from_slice(cb);
        cbw.resize(31, 0);
        self.bulk_out(ep_out, &cbw)?;
        let mut data = Vec::new();
        let mut stalled = false;
        if len > 0 {
            if dir_in {
                match self.bulk_data(ep_in, len, self.tag) {
                    Ok(d) => data = d,
                    Err(UsbErr::Stall(_)) => stalled = true,
                    Err(e) => return Err(e),
                }
            } else {
                self.bulk_out(ep_out, out)?;
            }
        }
        if stalled {
            // CLEAR_FEATURE(ENDPOINT_HALT): the computer's answer to a stalled bulk-in.
            self.control_write([0x02, 1, 0, 0, ep_in, 0, 0, 0], &[])?;
            self.stalled.retain(|e| *e != ep_in);
        }
        let csw = self.bulk_csw(ep_in)?;
        if !is_csw(&csw, self.tag) {
            return Err(UsbErr::Protocol("a bad command status wrapper".into()));
        }
        if &csw[0..4] != b"USBS" || u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]) != self.tag {
            return Err(UsbErr::Protocol("a bad command status wrapper".into()));
        }
        match csw[12] {
            0 if !stalled => Ok(data),
            2 => Err(UsbErr::Protocol("phase error".into())),
            _ => {
                let (k, a, q) = self.request_sense();
                Err(UsbErr::Scsi(k, a, q))
            }
        }
    }

    fn request_sense(&mut self) -> (u8, u8, u8) {
        match self.bot_raw(&[0x03, 0, 0, 0, 18, 0], 18) {
            Ok(d) if d.len() >= 14 => (d[2] & 0x0f, d[12], d[13]),
            _ => (0, 0, 0),
        }
    }

    /// A command whose own failure is not asked about (REQUEST SENSE's own).
    fn bot_raw(&mut self, cb: &[u8], len: usize) -> Result<Vec<u8>, UsbErr> {
        let (ep_in, ep_out) = (self.dev.ep_in, self.dev.ep_out);
        self.tag = self.tag.wrapping_add(1);
        let mut cbw = b"USBC".to_vec();
        cbw.extend_from_slice(&self.tag.to_le_bytes());
        cbw.extend_from_slice(&(len as u32).to_le_bytes());
        cbw.extend_from_slice(&[0x80, 0, cb.len() as u8]);
        cbw.extend_from_slice(cb);
        cbw.resize(31, 0);
        self.bulk_out(ep_out, &cbw)?;
        let data = self.bulk_data(ep_in, len, self.tag)?;
        self.bulk_csw(ep_in)?;
        Ok(data)
    }

    fn mount(&mut self) -> Result<(), UsbErr> {
        let intf = self.dev.intf;
        match self.control([0xA1, 0xFE, 0, 0, intf, 0, 1, 0], &[]) {
            Ok(d) => self.say(format!("GET_MAX_LUN: {}", d.first().copied().unwrap_or(0))),
            Err(UsbErr::Stall(_)) => self.say("GET_MAX_LUN stalled: one LUN".into()),
            Err(e) => return Err(e),
        }
        let inq = self.bot(&[0x12, 0, 0, 0, 36, 0], 36, true, &[])?;
        if inq.len() >= 32 {
            self.disk.vendor = String::from_utf8_lossy(&inq[8..16]).trim().to_string();
            self.disk.product = String::from_utf8_lossy(&inq[16..32]).trim().to_string();
        }
        self.say(format!("INQUIRY: {} {}", self.disk.vendor, self.disk.product));
        // TEST UNIT READY until the medium is there: a card can take a moment.
        let mut ready = false;
        for _ in 0..20 {
            match self.bot(&[0, 0, 0, 0, 0, 0], 0, true, &[]) {
                Ok(_) => {
                    ready = true;
                    break;
                }
                Err(UsbErr::Scsi(..)) => std::thread::sleep(Duration::from_millis(250)),
                Err(e) => return Err(e),
            }
        }
        if !ready {
            return Err(UsbErr::Scsi(2, 0x3A, 0));
        }
        self.say("TEST UNIT READY: ready".into());
        let cap = self.bot(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8, true, &[])?;
        if cap.len() < 8 {
            return Err(UsbErr::Protocol("READ CAPACITY answered short".into()));
        }
        let last = u32::from_be_bytes([cap[0], cap[1], cap[2], cap[3]]) as u64;
        let bs = u32::from_be_bytes([cap[4], cap[5], cap[6], cap[7]]);
        self.disk.blocks = last + 1;
        self.disk.block_size = bs;
        self.say(format!(
            "READ CAPACITY: {} blocks of {} bytes ({} MB)",
            self.disk.blocks,
            bs,
            (self.disk.blocks * bs as u64) >> 20
        ));
        if !(512..=4096).contains(&bs) || !bs.is_power_of_two() {
            return Err(UsbErr::Protocol(format!("unsupported FAT logical block size {bs}")));
        }
        // MODE SENSE(6): bit 7 of the device-specific parameter is write protect.
        if let Ok(ms) = self.bot(&[0x1A, 0, 0x3F, 0, 192, 0], 192, true, &[]) {
            self.disk.write_protected = ms.get(2).is_some_and(|b| b & 0x80 != 0);
        }
        self.say(format!("MODE SENSE: {}", if self.disk.write_protected { "write protected" } else { "writable" }));
        // PREVENT MEDIUM REMOVAL: a mounted volume is not to be pulled.
        let _ = self.bot(&[0x1E, 0, 0, 0, 1, 0], 0, true, &[]);
        self.stage = Stage::Mounted;
        self.publish(true);
        self.refresh_files()
    }

    /// The volume mounted, looked at, unmounted: what a file manager does on a click.
    fn refresh_files(&mut self) -> Result<(), UsbErr> {
        let t0 = Instant::now();
        let (r0, w0) = (self.read, self.written);
        let blocks = self.disk.blocks;
        let result = {
            let disk = ScsiDisk::open(self)?;
            list_volume(disk, blocks)
        };
        match result {
            Ok((label, files, free, total)) => {
                self.disk.label = label;
                self.say(format!(
                    "mounted FAT volume {:?}: {} entries, {} MB free of {} MB",
                    self.disk.label,
                    files.len(),
                    free >> 20,
                    total >> 20
                ));
                self.files = files;
                self.disk.free = free;
                self.disk.total = total;
                self.note_speed("listing", r0, w0, t0);
                Ok(())
            }
            Err(HostIo::Usb(e)) => Err(e),
            Err(HostIo::Fs(e)) => {
                self.say(format!("the computer cannot read the volume: {e}"));
                self.files.clear();
                self.last = format!("not readable: {e}");
                Ok(())
            }
        }
    }

    fn note_speed(&mut self, what: &str, r0: u64, w0: u64, t0: Instant) {
        let bytes = (self.read - r0) + (self.written - w0);
        let secs = t0.elapsed().as_secs_f64().max(0.001);
        self.speed = bytes as f64 / secs;
        self.last = format!("{what}: {} KB in {:.2} s ({:.2} MB/s)", bytes >> 10, secs, self.speed / 1e6);
    }

    fn disk_command(&mut self, c: Cmd) -> Result<(), UsbErr> {
        match c {
            Cmd::List => self.refresh_files(),
            Cmd::Put { name, data } => self.put(&name, &data),
            Cmd::PutRandom { name, size } => {
                let mut x = 0x2545_F491_4F6C_DD1Du64 ^ size as u64;
                let data: Vec<u8> = (0..size)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        (x >> 24) as u8
                    })
                    .collect();
                self.put(&name, &data)
            }
            Cmd::Get { name } => self.get(&name),
            Cmd::Eject => self.eject(),
            _ => Ok(()),
        }
    }

    fn put(&mut self, name: &str, data: &[u8]) -> Result<(), UsbErr> {
        if self.disk.write_protected {
            self.say(format!("copy {name:?}: the disk is write protected"));
            self.last = "the disk is write protected".into();
            return Ok(());
        }
        let t0 = Instant::now();
        let (r0, w0) = (self.read, self.written);
        self.say(format!("copying {name:?} ({} bytes) onto the disk", data.len()));
        self.publish(true);
        let blocks = self.disk.blocks;
        let result = {
            let disk = ScsiDisk::open(self)?;
            copy_in(disk, blocks, name, data)
        };
        match result {
            Ok(()) => {
                // A computer asks the disk to flush when it is done.
                let _ = self.bot(&[0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0], 0, true, &[]);
                self.note_speed(&format!("copied {name:?}"), r0, w0, t0);
                self.say(self.last.clone());
                let done = self.last.clone();
                let listed = self.refresh_files();
                self.last = done;
                listed
            }
            Err(HostIo::Usb(e)) => Err(e),
            Err(HostIo::Fs(e)) => {
                self.say(format!("copy {name:?} failed: {e}"));
                self.last = format!("copy failed: {e}");
                Ok(())
            }
        }
    }

    fn get(&mut self, name: &str) -> Result<(), UsbErr> {
        let t0 = Instant::now();
        let (r0, w0) = (self.read, self.written);
        let blocks = self.disk.blocks;
        let result = {
            let disk = ScsiDisk::open(self)?;
            read_file(disk, blocks, name)
        };
        match result {
            Ok(data) => {
                self.note_speed(&format!("read {name:?}"), r0, w0, t0);
                let line = format!("{} - {} bytes, crc32 {:08x}", self.last, data.len(), crc32(&data));
                self.last = line.clone();
                self.say(line);
                Ok(())
            }
            Err(HostIo::Usb(e)) => Err(e),
            Err(HostIo::Fs(e)) => {
                self.say(format!("read {name:?} failed: {e}"));
                self.last = format!("read failed: {e}");
                Ok(())
            }
        }
    }

    fn eject(&mut self) -> Result<(), UsbErr> {
        self.say("eject: ALLOW MEDIUM REMOVAL, SYNCHRONIZE CACHE, START STOP UNIT (eject)".into());
        let _ = self.bot(&[0x1E, 0, 0, 0, 0, 0], 0, true, &[]);
        let _ = self.bot(&[0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0], 0, true, &[]);
        match self.bot(&[0x1B, 0, 0, 0, 0x02, 0], 0, true, &[]) {
            Ok(_) => {
                self.stage = Stage::Ejected;
                self.say("ejected: it is safe to pull the cable out".into());
                self.last = "ejected".into();
                Ok(())
            }
            Err(UsbErr::Scsi(k, a, q)) => {
                self.say(format!("the disk refused to eject: {}", sense_text(k, a, q)));
                self.last = "the disk refused to eject".into();
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// A vowel under a dead key: ´ or ¨.
fn axes_json(axes: &BTreeMap<u16, i64>) -> Value {
    let mut value = json!({});
    for (usage, axis) in axes {
        let name = match *usage {
            hid::USAGE_X => "x",
            hid::USAGE_Y => "y",
            hid::USAGE_WHEEL => "wheel",
            _ => continue,
        };
        value[name] = json!(axis);
    }
    value
}

fn count_button(counts: &mut BTreeMap<u16, u64>, button: u16) -> Result<(), hid::Error> {
    let count = counts.entry(button).or_default();
    *count = count.checked_add(1).ok_or_else(|| hid::Error("mouse button transition counter overflow".into()))?;
    Ok(())
}

fn accent(dead: char, c: char) -> Option<char> {
    let (plain, acute, diaeresis) = ("aeiouAEIOU", "áéíóúÁÉÍÓÚ", "äëïöüÄËÏÖÜ");
    let at = plain.chars().position(|p| p == c)?;
    (if dead == '\u{b4}' { acute } else { diaeresis }).chars().nth(at)
}

/// Whether a frame is the status of command `tag`.
fn is_csw(f: &[u8], tag: u32) -> bool {
    f.len() == 13 && &f[0..4] == b"USBS" && u32::from_le_bytes([f[4], f[5], f[6], f[7]]) == tag
}

/// A key and what it types.
enum Key {
    Char(char),
    Backspace,
    None,
}

fn class_name(i: (u8, u8, u8)) -> &'static str {
    match i {
        (0x08, ..) => "mass storage",
        (0x03, 1, 1) => "keyboard",
        (0x03, ..) => "HID",
        (0x0e, ..) => "camera",
        (0x02, ..) => "serial",
        _ => "other",
    }
}

/// The character a boot-keyboard usage prints on a US or a Spanish (ISO) layout.
fn key_char(layout: &str, mods: u8, k: u8) -> Key {
    let shift = mods & 0x22 != 0;
    let altgr = mods & 0x40 != 0;
    let ctrl = mods & 0x11 != 0;
    if ctrl {
        return Key::None;
    }
    let letters = b"abcdefghijklmnopqrstuvwxyz";
    let c = match k {
        4..=29 => {
            let l = letters[(k - 4) as usize] as char;
            if shift {
                l.to_ascii_uppercase()
            } else {
                l
            }
        }
        30..=39 => {
            let n = (k - 29) % 10;
            let plain = char::from_digit(n as u32, 10).unwrap_or('0');
            let shifted_us = b"!@#$%^&*()"[((k - 30) as usize) % 10] as char;
            let shifted_es = [b'!', b'"', 0xB7, b'$', b'%', b'&', b'/', b'(', b')', b'='];
            if layout == "us" {
                if shift {
                    shifted_us
                } else {
                    plain
                }
            } else if altgr {
                match k {
                    30 => '|',
                    31 => '@',
                    32 => '#',
                    35 => '\u{ac}',
                    _ => '\0',
                }
            } else if shift {
                let c = shifted_es[(k - 30) as usize];
                if c == 0xB7 {
                    '\u{b7}'
                } else {
                    c as char
                }
            } else {
                plain
            }
        }
        40 => '\n',
        42 => return Key::Backspace,
        43 => '\t',
        44 => ' ',
        45..=56 | 100 => return punct(layout, shift, altgr, k),
        _ => return Key::None,
    };
    if c == '\0' {
        Key::None
    } else {
        Key::Char(c)
    }
}

fn punct(layout: &str, shift: bool, altgr: bool, k: u8) -> Key {
    let us = |p: char, s: char| Key::Char(if shift { s } else { p });
    if layout == "us" {
        return match k {
            45 => us('-', '_'),
            46 => us('=', '+'),
            47 => us('[', '{'),
            48 => us(']', '}'),
            49 => us('\\', '|'),
            51 => us(';', ':'),
            52 => us('\'', '"'),
            53 => us('`', '~'),
            54 => us(',', '<'),
            55 => us('.', '>'),
            56 => us('/', '?'),
            100 => us('\\', '|'),
            _ => Key::None,
        };
    }
    // Spanish, ISO
    let pick = |p: char, s: char, a: char| {
        Key::Char(if altgr && a != '\0' {
            a
        } else if shift {
            s
        } else {
            p
        })
    };
    match k {
        45 => pick('\'', '?', '\0'),
        46 => pick('\u{a1}', '\u{bf}', '\0'),
        47 => pick('`', '^', '['),
        48 => pick('+', '*', ']'),
        49 => pick('\u{e7}', '\u{c7}', '}'),
        51 => pick('\u{f1}', '\u{d1}', '\0'),
        52 => pick('\u{b4}', '\u{a8}', '{'),
        53 => pick('\u{ba}', '\u{aa}', '\\'),
        54 => pick(',', ';', '\0'),
        55 => pick('.', ':', '\0'),
        56 => pick('-', '_', '\0'),
        100 => pick('<', '>', '\0'),
        _ => Key::None,
    }
}

// ---- the disk as a block device, and the volume on it ---------------------

/// What can go wrong reading or writing the volume: the bus, or the filesystem.
enum HostIo {
    Usb(UsbErr),
    Fs(String),
}

impl From<UsbErr> for HostIo {
    fn from(e: UsbErr) -> Self {
        HostIo::Usb(e)
    }
}

/// The bus error a filesystem call carries, when it was one.
fn from_io(e: io::Error) -> HostIo {
    match e.get_ref().and_then(|i| i.downcast_ref::<UsbErrBox>()) {
        Some(b) => HostIo::Usb(b.0.clone()),
        None => HostIo::Fs(e.to_string()),
    }
}

#[derive(Debug)]
struct UsbErrBox(UsbErr);

impl std::fmt::Display for UsbErrBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for UsbErrBox {}

fn to_io(e: UsbErr) -> io::Error {
    io::Error::other(UsbErrBox(e))
}

/// The disk's sectors, over SCSI READ(10) and WRITE(10), with the cache a
/// computer's block layer has: 4 KB windows, written back together (and in
/// order, in runs) when the volume is flushed or the cache is full. A FAT
/// writes a few bytes of table for every cluster it fills; without this each
/// of them would be a command of its own.
struct ScsiDisk<'a> {
    host: &'a mut Host,
    pos: u64,
    cache: BTreeMap<u64, (Vec<u8>, bool)>,
    len: u64,
    sector_size: usize,
    /// The MBR partition's first sector, 0 when the disk is one volume.
    offset: u64,
}

/// Sectors in a window of the cache, and windows kept.
const WINDOW: usize = 8;
const WINDOWS: usize = 256;

impl<'a> ScsiDisk<'a> {
    fn open(host: &'a mut Host) -> Result<ScsiDisk<'a>, UsbErr> {
        let sector_size = host.disk.block_size as usize;
        let len = host.disk.blocks * sector_size as u64;
        let mut d = ScsiDisk { host, pos: 0, cache: BTreeMap::new(), len, sector_size, offset: 0 };
        let mbr = d.read_sectors(0, 1)?;
        d.offset = partition_start(&mbr);
        if d.offset >= d.host.disk.blocks {
            return Err(UsbErr::Protocol("partition starts outside the device".into()));
        }
        Ok(d)
    }

    fn read_sectors(&mut self, lba: u64, count: usize) -> Result<Vec<u8>, UsbErr> {
        let mut out = Vec::with_capacity(count * self.sector_size);
        let mut at = lba;
        let mut left = count;
        while left > 0 {
            let n = left.min(MAX_SECTORS);
            let cb =
                [0x28, 0, (at >> 24) as u8, (at >> 16) as u8, (at >> 8) as u8, at as u8, 0, (n >> 8) as u8, n as u8, 0];
            let d = self.host.bot(&cb, n * self.sector_size, true, &[])?;
            if d.len() != n * self.sector_size {
                return Err(UsbErr::Protocol("short SCSI block read".into()));
            }
            self.host.read += d.len() as u64;
            out.extend_from_slice(&d);
            at += n as u64;
            left -= n;
        }
        Ok(out)
    }

    fn write_sectors(&mut self, lba: u64, data: &[u8]) -> Result<(), UsbErr> {
        if !data.len().is_multiple_of(self.sector_size) {
            return Err(UsbErr::Protocol("unaligned SCSI block write".into()));
        }
        let mut at = lba;
        for part in data.chunks(MAX_SECTORS * self.sector_size) {
            let n = part.len() / self.sector_size;
            let cb =
                [0x2A, 0, (at >> 24) as u8, (at >> 16) as u8, (at >> 8) as u8, at as u8, 0, (n >> 8) as u8, n as u8, 0];
            self.host.bot(&cb, part.len(), false, part)?;
            self.host.written += part.len() as u64;
            at += n as u64;
        }
        Ok(())
    }

    /// Every dirty window to the disk, runs of neighbours as one write.
    fn flush_all(&mut self) -> Result<(), UsbErr> {
        let dirty: Vec<u64> = self.cache.iter().filter(|(_, w)| w.1).map(|(b, _)| *b).collect();
        let mut i = 0;
        while i < dirty.len() {
            let start = dirty[i];
            let mut data = self.cache[&start].0.clone();
            let mut j = i + 1;
            while j < dirty.len()
                && dirty[j] * self.sector_size as u64 == start * self.sector_size as u64 + data.len() as u64
            {
                data.extend_from_slice(&self.cache[&dirty[j]].0);
                j += 1;
            }
            self.write_sectors(start, &data)?;
            for b in &dirty[i..j] {
                if let Some(w) = self.cache.get_mut(b) {
                    w.1 = false;
                }
            }
            i = j;
        }
        Ok(())
    }

    /// The window holding sector `lba`, loaded.
    fn load(&mut self, lba: u64) -> Result<u64, UsbErr> {
        let base = lba - lba % WINDOW as u64;
        if self.cache.contains_key(&base) {
            return Ok(base);
        }
        if self.cache.len() >= WINDOWS {
            self.flush_all()?;
            self.cache.clear();
        }
        let count = WINDOW.min((self.len / self.sector_size as u64 - base) as usize);
        let data = self.read_sectors(base, count)?;
        self.cache.insert(base, (data, false));
        Ok(base)
    }
}

impl Read for ScsiDisk<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let abs = self.pos + self.offset * self.sector_size as u64;
        if abs >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let n = buf.len().min((self.len - abs) as usize);
        // Whole windows in bulk, straight to the buffer.
        if abs.is_multiple_of((WINDOW * self.sector_size) as u64) && n >= WINDOW * self.sector_size {
            let sectors = n / self.sector_size / WINDOW * WINDOW;
            self.flush_all().map_err(to_io)?;
            let d = self.read_sectors(abs / self.sector_size as u64, sectors).map_err(to_io)?;
            buf[..d.len()].copy_from_slice(&d);
            self.pos += d.len() as u64;
            return Ok(d.len());
        }
        let base = self.load(abs / self.sector_size as u64).map_err(to_io)?;
        let data = &self.cache[&base].0;
        let from = (abs - base * self.sector_size as u64) as usize;
        let k = n.min(data.len() - from);
        buf[..k].copy_from_slice(&data[from..from + k]);
        self.pos += k as u64;
        Ok(k)
    }
}

impl Write for ScsiDisk<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let abs = self.pos + self.offset * self.sector_size as u64;
        if abs >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let n = buf.len().min((self.len - abs) as usize);
        if abs.is_multiple_of((WINDOW * self.sector_size) as u64) && n >= WINDOW * self.sector_size {
            // Whole windows in bulk: what the cache holds of them is stale.
            let whole = n / (WINDOW * self.sector_size) * (WINDOW * self.sector_size);
            self.flush_all().map_err(to_io)?;
            let (first, last) = (abs / self.sector_size as u64, (abs + whole as u64) / self.sector_size as u64);
            self.cache.retain(|b, _| *b < first || *b >= last);
            self.write_sectors(first, &buf[..whole]).map_err(to_io)?;
            self.pos += whole as u64;
            return Ok(whole);
        }
        let base = self.load(abs / self.sector_size as u64).map_err(to_io)?;
        let w = self.cache.get_mut(&base).expect("loaded");
        let from = (abs - base * self.sector_size as u64) as usize;
        let k = n.min(w.0.len() - from);
        w.0[from..from + k].copy_from_slice(&buf[..k]);
        w.1 = true;
        self.pos += k as u64;
        Ok(k)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_all().map_err(to_io)
    }
}

impl Seek for ScsiDisk<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let size = self.len - self.offset * self.sector_size as u64;
        let p = match to {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(d) => size as i64 + d,
            SeekFrom::Current(d) => self.pos as i64 + d,
        };
        if p < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"));
        }
        self.pos = p as u64;
        Ok(self.pos)
    }
}

impl Drop for ScsiDisk<'_> {
    fn drop(&mut self) {
        let _ = self.flush_all();
    }
}

/// Where the first partition starts: an MBR's entry, or 0 for a volume with no MBR.
fn partition_start(sector0: &[u8]) -> u64 {
    if sector0.len() < 512 || sector0[510] != 0x55 || sector0[511] != 0xAA {
        return 0;
    }
    // A FAT boot sector has the same signature: its jump byte tells it from an MBR.
    if matches!(sector0[0], 0xEB | 0xE9) {
        return 0;
    }
    for i in 0..4 {
        let e = &sector0[446 + 16 * i..446 + 16 * (i + 1)];
        if e[4] != 0 {
            return u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64;
        }
    }
    0
}

/// The time on the files the computer writes: the Mac's clock.
#[derive(Debug)]
struct MacClock;

impl fatfs::TimeProvider for MacClock {
    fn get_current_date(&self) -> fatfs::Date {
        self.get_current_date_time().date
    }

    fn get_current_date_time(&self) -> fatfs::DateTime {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let (days, rem) = ((secs / 86400) as i64, (secs % 86400) as u32);
        // Civil date from days since 1970 (Howard Hinnant's algorithm).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u16;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u16;
        let y = (yoe + era * 400 + i64::from(m <= 2)) as u16;
        fatfs::DateTime {
            date: fatfs::Date { year: y.max(1980), month: m, day: d },
            time: fatfs::Time {
                hour: (rem / 3600) as u16,
                min: ((rem / 60) % 60) as u16,
                sec: (rem % 60) as u16,
                millis: 0,
            },
        }
    }
}

static CLOCK: MacClock = MacClock;

fn fs_of(disk: ScsiDisk<'_>) -> Result<fatfs::FileSystem<ScsiDisk<'_>>, HostIo> {
    fatfs::FileSystem::new(disk, fatfs::FsOptions::new().time_provider(&CLOCK)).map_err(from_io)
}

type Listing = (String, Vec<FileInfo>, u64, u64);

fn list_volume(disk: ScsiDisk<'_>, _blocks: u64) -> Result<Listing, HostIo> {
    let fs = fs_of(disk)?;
    let label = fs.volume_label();
    let st = fs.stats().map_err(from_io)?;
    let (free, total) =
        (st.free_clusters() as u64 * st.cluster_size() as u64, st.total_clusters() as u64 * st.cluster_size() as u64);
    let mut files = Vec::new();
    walk(&fs.root_dir(), "", 0, &mut files).map_err(from_io)?;
    Ok((label, files, free, total))
}

fn walk(dir: &fatfs::Dir<'_, ScsiDisk<'_>>, prefix: &str, depth: usize, out: &mut Vec<FileInfo>) -> io::Result<()> {
    for entry in dir.iter() {
        let e = entry?;
        let name = e.file_name();
        if name == "." || name == ".." || out.len() >= 200 {
            continue;
        }
        let path = format!("{prefix}{name}");
        out.push(FileInfo { name: path.clone(), size: e.len(), dir: e.is_dir() });
        if e.is_dir() && depth < 2 {
            walk(&e.to_dir(), &format!("{path}/"), depth + 1, out)?;
        }
    }
    Ok(())
}

fn copy_in(disk: ScsiDisk<'_>, _blocks: u64, name: &str, data: &[u8]) -> Result<(), HostIo> {
    let fs = fs_of(disk)?;
    let root = fs.root_dir();
    let mut f = root.create_file(name).map_err(from_io)?;
    f.truncate().map_err(from_io)?;
    f.write_all(data).map_err(from_io)?;
    f.flush().map_err(from_io)?;
    drop(f);
    drop(root);
    fs.unmount().map_err(from_io)
}

fn read_file(disk: ScsiDisk<'_>, _blocks: u64, name: &str) -> Result<Vec<u8>, HostIo> {
    let fs = fs_of(disk)?;
    let root = fs.root_dir();
    let mut f = root.open_file(name).map_err(from_io)?;
    let mut data = Vec::new();
    f.read_to_end(&mut data).map_err(from_io)?;
    Ok(data)
}

/// CRC-32 (IEEE), to say what a read gave back.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for b in data {
        crc ^= *b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A USB mass-storage device as the firmware's TinyUSB presents one to
    /// kemu's controller: it answers the frames the computer sends the way
    /// the stack would (control transfers cut in 64-byte packets, BOT, SCSI).
    struct Fake {
        disk: Vec<u8>,
        block_size: usize,
        protect: bool,
        /// Test unit ready fails this many times before the medium is there.
        not_ready: u32,
        ejected: bool,
        sense: (u8, u8, u8),
        // the bulk-out state machine
        cbw: Option<(u32, usize, bool, Vec<u8>)>,
        out: Vec<u8>,
        keyboard: bool,
        protocol_stall: bool,
        configuration: Option<Vec<u8>>,
        reports: BTreeMap<u8, Vec<u8>>,
        flushed: u32,
    }

    fn msc_config() -> Vec<u8> {
        let mut c = vec![9, 2, 32, 0, 1, 1, 0, 0x80, 50];
        c.extend_from_slice(&[9, 4, 0, 0, 2, 8, 6, 0x50, 0]);
        c.extend_from_slice(&[7, 5, 0x81, 2, 0, 2, 0]);
        c.extend_from_slice(&[7, 5, 0x02, 2, 0, 2, 0]);
        c
    }

    fn kbd_config() -> Vec<u8> {
        let report_len = hid::test_support::keyboard().len() as u16;
        let mut c = vec![9, 2, 34, 0, 1, 1, 0, 0x80, 50];
        c.extend_from_slice(&[9, 4, 0, 0, 1, 3, 1, 1, 0]);
        c.extend_from_slice(&[9, 0x21, 0x11, 1, 0, 1, 0x22, report_len as u8, (report_len >> 8) as u8]);
        c.extend_from_slice(&[7, 5, 0x81, 3, 8, 0, 10]);
        c
    }

    fn string(s: &str) -> Vec<u8> {
        let u: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut d = vec![(u.len() + 2) as u8, 3];
        d.extend(u);
        d
    }

    impl Fake {
        fn new(disk: Vec<u8>) -> Fake {
            Fake {
                disk,
                block_size: 512,
                protect: false,
                not_ready: 0,
                ejected: false,
                sense: (0, 0, 0),
                cbw: None,
                out: vec![],
                keyboard: false,
                protocol_stall: false,
                configuration: None,
                reports: BTreeMap::new(),
                flushed: 0,
            }
        }

        fn control_in(&self, mut data: Vec<u8>, w_length: usize) -> Vec<(u8, Vec<u8>)> {
            data.truncate(w_length);
            let mut out = Vec::new();
            let mut chunks: Vec<&[u8]> = data.chunks(64).collect();
            if data.len().is_multiple_of(64) && data.len() < w_length {
                chunks.push(&[]);
            }
            if chunks.is_empty() {
                chunks.push(&[]);
            }
            for c in chunks {
                let mut p = vec![0x80];
                p.extend_from_slice(c);
                out.push((proto::USB_IN, p));
            }
            out
        }

        fn frame(&mut self, t: u8, p: &[u8]) -> Vec<(u8, Vec<u8>)> {
            match t {
                proto::USB_BUS if p[0] == BUS_CABLE_IN => vec![(proto::USB_DEV, vec![DEV_PULLUP, 1, 0, 0])],
                proto::USB_BUS if p[0] == BUS_RESET => vec![],
                proto::USB_BUS => vec![(proto::USB_DEV, vec![DEV_PULLUP, 0, 0, 0])],
                proto::USB_SETUP => self.setup(p),
                proto::USB_OUT if p[0] == 0 => vec![],
                proto::USB_OUT => {
                    let ep = p[0];
                    let mut r = vec![(proto::USB_ACK, [&[ep][..], &((p.len() - 1) as u32).to_le_bytes()].concat())];
                    r.extend(self.bulk_out(&p[1..]));
                    r
                }
                _ => vec![],
            }
        }

        fn setup(&mut self, s: &[u8]) -> Vec<(u8, Vec<u8>)> {
            let w = u16::from_le_bytes([s[6], s[7]]) as usize;
            let status = vec![(proto::USB_IN, vec![0x80])];
            match (s[0], s[1], s[3]) {
                (0x21, 0x0b, _) if self.protocol_stall => vec![(proto::USB_DEV, vec![DEV_STALL, 0x80, 0, 0])],
                (0x80, 6, 1) => {
                    let d = vec![18, 1, 0, 2, 0, 0, 0, 64, 0x6a, 0x30, 0x34, 0x12, 0, 1, 1, 2, 0, 1];
                    self.control_in(d, w)
                }
                (0x80, 6, 2) => self.control_in(
                    self.configuration
                        .clone()
                        .unwrap_or_else(|| if self.keyboard { kbd_config() } else { msc_config() }),
                    w,
                ),
                (0x80, 6, 3) => self.control_in(string(if s[2] == 1 { "Kode" } else { "Kode Dot SD Reader" }), w),
                (0x81, 6, 0x22) => {
                    let report = if self.configuration.is_some() {
                        self.reports.get(&s[4]).cloned()
                    } else {
                        (self.keyboard && s[4] == 0).then(hid::test_support::keyboard)
                    };
                    match report {
                        Some(report) if s[2] == 0 && s[5] == 0 => self.control_in(report, w),
                        _ => vec![(proto::USB_DEV, vec![DEV_STALL, 0x80, 0, 0])],
                    }
                }
                (0xA1, 0xFE, _) => self.control_in(vec![0], w),
                (0x00, ..) | (0x21, ..) | (0x02, ..) => status,
                _ => vec![(proto::USB_DEV, vec![DEV_STALL, 0x80, 0, 0])],
            }
        }

        fn csw(tag: u32, residue: u32, status: u8) -> (u8, Vec<u8>) {
            let mut p = vec![0x81];
            p.extend_from_slice(b"USBS");
            p.extend_from_slice(&tag.to_le_bytes());
            p.extend_from_slice(&residue.to_le_bytes());
            p.push(status);
            (proto::USB_IN, p)
        }

        fn bulk_out(&mut self, data: &[u8]) -> Vec<(u8, Vec<u8>)> {
            self.out.extend_from_slice(data);
            if self.cbw.is_none() {
                if self.out.len() < 31 {
                    return vec![];
                }
                let c = std::mem::take(&mut self.out);
                assert_eq!(&c[0..4], b"USBC");
                let tag = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
                let len = u32::from_le_bytes([c[8], c[9], c[10], c[11]]) as usize;
                let dir_in = c[12] & 0x80 != 0;
                let cb = c[15..15 + c[14] as usize].to_vec();
                if len > 0 && !dir_in {
                    self.cbw = Some((tag, len, dir_in, cb));
                    return vec![];
                }
                return self.scsi(tag, &cb, &[]);
            }
            let need = self.cbw.as_ref().expect("a cbw").1;
            if self.out.len() < need {
                return vec![];
            }
            let (tag, _, _, cb) = self.cbw.take().expect("a cbw");
            let d = std::mem::take(&mut self.out);
            self.scsi(tag, &cb, &d)
        }

        fn scsi(&mut self, tag: u32, cb: &[u8], data: &[u8]) -> Vec<(u8, Vec<u8>)> {
            let reply = |d: Vec<u8>| -> Vec<(u8, Vec<u8>)> {
                let mut v = vec![];
                if !d.is_empty() {
                    v.push((proto::USB_IN, [&[0x81][..], &d].concat()));
                }
                v.push(Fake::csw(tag, 0, 0));
                v
            };
            let lba = u32::from_be_bytes([
                cb.get(2).copied().unwrap_or(0),
                cb.get(3).copied().unwrap_or(0),
                cb.get(4).copied().unwrap_or(0),
                cb.get(5).copied().unwrap_or(0),
            ]) as usize;
            let n = u16::from_be_bytes([cb.get(7).copied().unwrap_or(0), cb.get(8).copied().unwrap_or(0)]) as usize;
            match cb[0] {
                0x00 => {
                    if self.not_ready > 0 {
                        self.not_ready -= 1;
                        self.sense = (2, 0x3A, 0);
                        return vec![Fake::csw(tag, 0, 1)];
                    }
                    if self.ejected {
                        self.sense = (2, 0x3A, 0);
                        return vec![Fake::csw(tag, 0, 1)];
                    }
                    reply(vec![])
                }
                0x03 => {
                    let mut d =
                        vec![0x70, 0, self.sense.0, 0, 0, 0, 0, 10, 0, 0, 0, 0, self.sense.1, self.sense.2, 0, 0, 0, 0];
                    d.truncate(18);
                    self.sense = (0, 0, 0);
                    reply(d)
                }
                0x12 => {
                    let mut d = vec![0, 0x80, 2, 2, 31, 0, 0, 0];
                    d.extend_from_slice(b"KODE    ");
                    d.extend_from_slice(b"DOT SD READER   ");
                    d.extend_from_slice(b"1.0 ");
                    reply(d)
                }
                0x25 => {
                    let blocks = self.disk.len().checked_div(self.block_size).unwrap_or(0);
                    let mut d = (blocks.saturating_sub(1) as u32).to_be_bytes().to_vec();
                    d.extend_from_slice(&(self.block_size as u32).to_be_bytes());
                    reply(d)
                }
                0x1A => reply(vec![3, 0, if self.protect { 0x80 } else { 0 }, 0]),
                0x28 => reply(self.disk[lba * self.block_size..(lba + n) * self.block_size].to_vec()),
                0x2A => {
                    assert_eq!(data.len(), n * self.block_size);
                    self.disk[lba * self.block_size..(lba + n) * self.block_size].copy_from_slice(data);
                    reply(vec![])
                }
                0x35 => {
                    self.flushed += 1;
                    reply(vec![])
                }
                0x1E => reply(vec![]),
                0x1B => {
                    if cb[4] & 3 == 2 {
                        self.ejected = true;
                    }
                    reply(vec![])
                }
                _ => {
                    self.sense = (5, 0x20, 0);
                    vec![Fake::csw(tag, 0, 1)]
                }
            }
        }
    }

    /// A FAT volume of `mb` megabytes with one file in it.
    fn volume(mb: usize) -> Vec<u8> {
        volume_with_sector_size(mb, 512)
    }

    fn volume_with_sector_size(mb: usize, sector_size: u16) -> Vec<u8> {
        let mut img = io::Cursor::new(vec![0u8; mb << 20]);
        fatfs::format_volume(
            &mut img,
            fatfs::FormatVolumeOptions::new().bytes_per_sector(sector_size).volume_label(*b"KODE_CARD  "),
        )
        .expect("format");
        {
            let fs = fatfs::FileSystem::new(&mut img, fatfs::FsOptions::new()).expect("mount");
            let mut f = fs.root_dir().create_file("hello.txt").expect("create");
            f.write_all(b"hello from the card").expect("write");
        }
        img.into_inner()
    }

    /// The frames the host put on the wire: (type, payload).
    type Wired = Arc<Mutex<Vec<(u8, Vec<u8>)>>>;

    struct Rig {
        host: UsbHost,
        fake: Arc<Mutex<Fake>>,
        snaps: Arc<Mutex<Vec<Value>>>,
        wired: Wired,
    }

    fn rig(fake: Fake) -> Rig {
        let (tx, rx) = mpsc::channel();
        let fake = Arc::new(Mutex::new(fake));
        let wired = Arc::new(Mutex::new(Vec::new()));
        let (f2, tx2, w2) = (fake.clone(), tx.clone(), wired.clone());
        let wire: Wire = Arc::new(move |t, p| {
            w2.lock().unwrap().push((t, p.to_vec()));
            for (rt, rp) in f2.lock().unwrap().frame(t, p) {
                if let Some(ev) = decode(rt, &rp) {
                    let _ = tx2.send(ev);
                }
            }
        });
        let snaps = Arc::new(Mutex::new(Vec::new()));
        let s2 = snaps.clone();
        let emit: Emit = Arc::new(move |j| s2.lock().unwrap().push(j));
        let host = Host::new(rx, wire, emit);
        std::thread::spawn(move || host.run());
        Rig { host: UsbHost { tx }, fake, snaps, wired }
    }

    impl Rig {
        fn last(&self) -> Value {
            self.snaps.lock().unwrap().last().cloned().unwrap_or(Value::Null)["usb"].clone()
        }

        fn wait(&self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
            let t0 = Instant::now();
            loop {
                let u = self.last();
                if f(&u) {
                    return u;
                }
                assert!(t0.elapsed() < Duration::from_secs(20), "waiting for {what}: {u}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn stage(&self, s: &str) -> Value {
            self.wait(s, |u| u["stage"] == s)
        }
    }

    #[test]
    fn a_plugged_in_disk_is_enumerated_and_its_volume_listed() {
        let r = rig(Fake::new(volume(4)));
        r.host.command(Cmd::Plug);
        let u = r.stage("mounted");
        assert_eq!(u["device"]["vid"], "306a");
        assert_eq!(u["device"]["product"], "Kode Dot SD Reader");
        assert_eq!(u["device"]["class"], "mass storage");
        assert_eq!(u["disk"]["vendor"], "KODE");
        assert_eq!(u["disk"]["block_size"], 512);
        assert_eq!(u["disk"]["write_protected"], false);
        let u = r.wait("the files", |u| u["disk"]["files"].as_array().is_some_and(|f| !f.is_empty()));
        assert_eq!(u["disk"]["label"], "KODE_CARD");
        assert_eq!(u["disk"]["files"][0]["name"], "hello.txt");
        assert_eq!(u["disk"]["files"][0]["size"], 19);
        // The computer enumerated as one does: address, then configuration.
        let w = r.wired.lock().unwrap();
        let setups: Vec<&Vec<u8>> = w.iter().filter(|f| f.0 == proto::USB_SETUP).map(|f| &f.1).collect();
        assert_eq!(setups[0][1], 6, "GET_DESCRIPTOR first");
        assert!(setups.iter().any(|s| s[1] == 5 && s[2] == 1), "SET_ADDRESS 1");
        assert!(setups.iter().any(|s| s[1] == 9 && s[2] == 1), "SET_CONFIGURATION 1");
    }

    #[test]
    fn a_bus_reset_enumerates_the_same_disk_without_toggling_its_cable() {
        let r = rig(Fake::new(volume(4)));
        r.host.command(Cmd::Plug);
        r.stage("mounted");
        r.host.command(Cmd::from_json(&json!({"cmd": "reset"})).unwrap());
        let u = r.wait("the reset and second enumeration", |u| {
            u["stage"] == "mounted"
                && u["disk"]["files"].as_array().is_some_and(|files| files.iter().any(|f| f["name"] == "hello.txt"))
                && u["log"].as_array().unwrap().iter().filter(|l| l.as_str().unwrap().contains("SET_ADDRESS 1")).count()
                    == 2
        });
        assert_eq!(u["cable"], true);
        assert_eq!(u["disk"]["files"][0]["name"], "hello.txt");
        let wire = r.wired.lock().unwrap();
        let bus: Vec<&Vec<u8>> = wire.iter().filter(|f| f.0 == proto::USB_BUS).map(|f| &f.1).collect();
        assert_eq!(bus, vec![&vec![BUS_CABLE_IN], &vec![BUS_RESET]]);
    }

    #[test]
    fn a_disk_not_ready_at_first_is_waited_for() {
        let mut f = Fake::new(volume(4));
        f.not_ready = 2;
        let r = rig(f);
        r.host.command(Cmd::Plug);
        r.stage("mounted");
    }

    #[test]
    fn a_file_copied_in_is_there_on_the_disk_and_reads_back_the_same() {
        let r = rig(Fake::new(volume(4)));
        r.host.command(Cmd::Plug);
        r.stage("mounted");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 + i / 251) as u8).collect();
        r.host.command(Cmd::Put { name: "big.bin".into(), data: data.clone() });
        let u = r.wait("the copy", |u| u["last"].as_str().is_some_and(|l| l.starts_with("copied")));
        assert!(u["written"].as_u64().unwrap() >= 300_000);
        let files = u["disk"]["files"].as_array().unwrap().clone();
        assert!(files.iter().any(|f| f["name"] == "big.bin" && f["size"] == 300_000), "{files:?}");
        assert!(r.fake.lock().unwrap().flushed >= 1, "the computer asks the disk to flush");
        r.host.command(Cmd::Get { name: "big.bin".into() });
        let u = r.wait("the read", |u| u["last"].as_str().is_some_and(|l| l.contains("crc32")));
        assert!(u["last"].as_str().unwrap().contains(&format!("{:08x}", crc32(&data))), "{}", u["last"]);
        // What is on the disk is a volume anyone can mount.
        let mut img = io::Cursor::new(r.fake.lock().unwrap().disk.clone());
        let fs = fatfs::FileSystem::new(&mut img, fatfs::FsOptions::new()).expect("mount");
        let mut got = Vec::new();
        fs.root_dir().open_file("big.bin").expect("open").read_to_end(&mut got).expect("read");
        assert_eq!(got, data);
        let mut hello = String::new();
        fs.root_dir().open_file("hello.txt").expect("open").read_to_string(&mut hello).expect("read");
        assert_eq!(hello, "hello from the card");
    }

    #[test]
    fn fat_io_uses_the_logical_block_size_reported_by_the_device() {
        for block_size in [512, 1024, 2048, 4096] {
            let mut device = Fake::new(volume_with_sector_size(4, block_size));
            device.block_size = usize::from(block_size);
            let r = rig(device);
            r.host.command(Cmd::Plug);
            let u = r.stage("mounted");
            assert_eq!(u["disk"]["block_size"], block_size);
            let data: Vec<_> = (0..usize::from(block_size) * WINDOW + 3).map(|i| (i % 251) as u8).collect();
            r.host.command(Cmd::Put { name: "blocks.bin".into(), data: data.clone() });
            r.wait("file copied with the declared block size", |u| {
                u["disk"]["files"]
                    .as_array()
                    .is_some_and(|files| files.iter().any(|f| f["name"] == "blocks.bin" && f["size"] == data.len()))
            });
            r.host.command(Cmd::Get { name: "blocks.bin".into() });
            r.wait("file read with the declared block size", |u| {
                u["last"].as_str().is_some_and(|line| line.contains(&format!("crc32 {:08x}", crc32(&data))))
            });
            let mut image = io::Cursor::new(r.fake.lock().unwrap().disk.clone());
            let fs = fatfs::FileSystem::new(&mut image, fatfs::FsOptions::new()).unwrap();
            let mut actual = Vec::new();
            fs.root_dir().open_file("blocks.bin").unwrap().read_to_end(&mut actual).unwrap();
            assert_eq!(actual, data);
        }
    }

    #[test]
    fn invalid_logical_block_sizes_are_rejected_before_fat_io() {
        for block_size in [0, 256, 768, 8192] {
            let mut device = Fake::new(volume(4));
            device.block_size = block_size;
            let r = rig(device);
            r.host.command(Cmd::Plug);
            let u = r.stage("failed");
            assert!(u["error"].as_str().unwrap().contains("unsupported FAT logical block size"));
        }
    }

    #[test]
    fn a_write_protected_disk_is_not_written() {
        let mut f = Fake::new(volume(4));
        f.protect = true;
        let r = rig(f);
        r.host.command(Cmd::Plug);
        let u = r.stage("mounted");
        assert_eq!(u["disk"]["write_protected"], true);
        let before = r.fake.lock().unwrap().disk.clone();
        r.host.command(Cmd::Put { name: "x.txt".into(), data: b"x".to_vec() });
        r.wait("the refusal", |u| u["last"].as_str().is_some_and(|l| l.contains("write protected")));
        assert_eq!(r.fake.lock().unwrap().disk, before);
    }

    #[test]
    fn ejecting_stops_the_disk_and_the_cable_can_be_pulled() {
        let r = rig(Fake::new(volume(4)));
        r.host.command(Cmd::Plug);
        r.stage("mounted");
        r.host.command(Cmd::Eject);
        r.stage("ejected");
        assert!(r.fake.lock().unwrap().ejected);
        // The stack stopped, as SD Reader's does once it is ejected: the pull-up drops, and the page still says ejected.
        r.host.firmware_said(proto::USB_DEV, &[DEV_PULLUP, 0, 0, 0]);
        r.wait("the pull-up", |u| {
            u["log"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("drops its pull-up"))
        });
        assert_eq!(r.last()["stage"], "ejected");
        r.host.command(Cmd::Unplug);
        let u = r.stage("no_cable");
        assert_eq!(u["cable"], false);
        assert!(u["log"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("after ejecting")));
    }

    #[test]
    fn pulling_the_cable_without_ejecting_lets_go_of_the_disk() {
        let r = rig(Fake::new(volume(4)));
        r.host.command(Cmd::Plug);
        r.stage("mounted");
        r.host.command(Cmd::Unplug);
        let u = r.stage("no_cable");
        assert!(u["disk"].is_null());
        assert!(r.wired.lock().unwrap().iter().any(|f| f.0 == proto::USB_BUS && f.1 == [BUS_CABLE_OUT]));
        // And plugged in again it is found again.
        r.host.command(Cmd::Plug);
        r.stage("mounted");
    }

    #[test]
    fn a_restarted_kode_dot_finds_the_cable_where_it_was_and_is_enumerated_again() {
        let r = rig(Fake::new(volume(4)));
        r.host.command(Cmd::Plug);
        r.stage("mounted");
        r.fake.lock().unwrap().ejected = false;
        r.host.rebooted();
        // The new boot raises its pull-up at once (the fake's does): the cable
        // was announced to it, and the computer enumerates it again.
        r.wait("a second enumeration", |u| {
            u["stage"] == "mounted"
                && u["log"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|l| l.as_str().unwrap().contains("SET_CONFIGURATION"))
                    .count()
                    == 2
        });
        let n = r.wired.lock().unwrap().iter().filter(|f| f.0 == proto::USB_BUS && f.1 == [BUS_CABLE_IN]).count();
        assert_eq!(n, 2, "the cable is announced to the new boot");
    }

    #[test]
    fn data_left_from_before_a_restart_is_not_taken_for_the_new_boot_s() {
        let r = rig(Fake::new(volume(4)));
        r.host.command(Cmd::Plug);
        r.stage("mounted");
        {
            // Held, the fake cannot raise the new boot's pull-up until a
            // sector read before the restart is in: the order a busy machine
            // can give them. Data carries no tag to tell it by.
            let _held = r.fake.lock().unwrap();
            r.host.rebooted();
            let mut stale = vec![0x81];
            stale.extend_from_slice(&[0xAB; 64]);
            r.host.firmware_said(proto::USB_IN, &stale);
        }
        r.wait("a second enumeration", |u| {
            u["stage"] == "mounted"
                && u["log"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|l| l.as_str().unwrap().contains("SET_CONFIGURATION"))
                    .count()
                    == 2
        });
    }

    #[test]
    fn a_keyboard_types_what_it_reports_on_either_layout() {
        let mut f = Fake::new(vec![]);
        f.keyboard = true;
        let r = rig(f);
        r.host.command(Cmd::Plug);
        let u = r.stage("keyboard");
        assert_eq!(u["device"]["class"], "keyboard");
        let press = |mods: u8, key: u8| {
            r.host.firmware_said(proto::USB_IN, &[0x81, mods, 0, key, 0, 0, 0, 0, 0]);
            r.host.firmware_said(proto::USB_IN, &[0x81, 0, 0, 0, 0, 0, 0, 0, 0]);
        };
        // "Hi" + shift-7 + AltGr-2 on a Spanish layout
        press(2, 11);
        press(0, 12);
        press(2, 36);
        press(0x40, 31);
        press(0, 51);
        let u = r.wait("the text", |u| u["typed"].as_str().is_some_and(|t| t.chars().count() >= 5));
        assert_eq!(u["typed"], "Hi/@\u{f1}");
        r.host.command(Cmd::Layout("us".into()));
        r.host.command(Cmd::ClearTyped);
        r.wait("cleared", |u| u["typed"] == "" && u["layout"] == "us");
        press(2, 36);
        press(2, 31);
        press(0, 51);
        let u = r.wait("the text", |u| u["typed"].as_str().is_some_and(|t| t.chars().count() >= 3));
        assert_eq!(u["typed"], "&@;");
        r.host.command(Cmd::Layout("es".into()));
        r.host.command(Cmd::ClearTyped);
        r.wait("cleared", |u| u["typed"] == "" && u["layout"] == "es");
        // Qué: the acute is a dead key on the Spanish layout; ´ then space prints it alone
        press(2, 20);
        press(0, 24);
        press(0, 52);
        press(0, 8);
        press(0, 52);
        press(0, 44);
        let u = r.wait("the text", |u| u["typed"].as_str().is_some_and(|t| t.chars().count() >= 4));
        assert_eq!(u["typed"], "Qu\u{e9}\u{b4} ");
    }

    fn composite_fake(interfaces: &[(u8, u8, Vec<u8>)]) -> Fake {
        let mut f = Fake::new(vec![]);
        let mut cfg = vec![9, 2, 0, 0, interfaces.len() as u8, 1, 0, 0x80, 50];
        for (number, endpoint, report) in interfaces {
            let length = (report.len() as u16).to_le_bytes();
            cfg.extend([9, 4, *number, 0, 1, 3, 0, 0, 0]);
            cfg.extend([9, 0x21, 0x11, 1, 0, 1, 0x22, length[0], length[1]]);
            cfg.extend([7, 5, *endpoint, 3, 64, 0, 1]);
            f.reports.insert(*number, report.clone());
        }
        let length = (cfg.len() as u16).to_le_bytes();
        cfg[2..4].copy_from_slice(&length);
        f.configuration = Some(cfg);
        f
    }

    #[test]
    fn separate_hid_interfaces_route_interleaved_reports_and_reset_independently() {
        let r = rig(composite_fake(&[(7, 0x86, hid::test_support::mouse()), (4, 0x83, hid::test_support::keyboard())]));
        r.host.command(Cmd::Plug);
        let u = r.stage("hid");
        assert_eq!(u["device"]["class"], "composite");
        assert_eq!(u["hid"]["keyboard"], true);
        assert_eq!(u["hid"]["mouse"], true);
        assert_eq!(u["interfaces"][0]["buttons"], json!([1, 2, 3, 4, 5]));
        assert_eq!(u["interfaces"][1]["buttons"], json!([]));
        assert_eq!(u["interfaces"][0]["number"], 7);
        assert_eq!(u["interfaces"][0]["endpoints"][0]["address"], 0x86);
        r.host.firmware_said(proto::USB_IN, &[0x83, 2, 0, 4, 0, 0, 0, 0, 0]);
        r.host.firmware_said(proto::USB_IN, &[0x86, 5, 12, 254, 1]);
        r.host.firmware_said(proto::USB_IN, &[0x83, 0, 0, 0, 0, 0, 0, 0, 0]);
        let u = r.wait("keyboard release and mouse delta", |u| {
            u["typed"] == "A" && u["hid"]["reports"].as_array().is_some_and(|r| r.len() == 2)
        });
        assert_eq!(u["keys_down"], json!([]));
        assert_eq!(u["modifiers"], 0);
        assert_eq!(u["mouse"]["buttons"], json!([1, 3]));
        assert_eq!(u["hid"]["reports"][1]["interface"], 7);
        assert_eq!(u["hid"]["reports"][1]["mouse"]["total"], json!({"x":12,"y":-2,"wheel":1}));
        r.host.firmware_said(proto::USB_IN, &[0x86, 0, 0, 0, 0]);
        r.wait("mouse release", |u| u["mouse"]["buttons"] == json!([]));
        r.host.command(Cmd::Reset);
        let u = r.wait("reset enumeration", |u| u["stage"] == "hid" && u["hid"]["reports"] == json!([]));
        assert_eq!(u["keys_down"], json!([]));
        assert_eq!(u["mouse"]["buttons"], json!([]));
        let wired = r.wired.lock().unwrap();
        for number in [4, 7] {
            assert!(wired
                .iter()
                .any(|(t, s)| *t == proto::USB_SETUP && s[0] == 0x81 && s[3] == 0x22 && s[4] == number));
        }
    }

    #[test]
    fn shared_endpoint_uses_declared_ids_and_invalid_input_is_transactional() {
        let mut report = hid::test_support::numbered(hid::test_support::keyboard(), 37);
        report.extend(hid::test_support::numbered(hid::test_support::mouse(), 193));
        let r = rig(composite_fake(&[(9, 0x84, report)]));
        r.host.command(Cmd::Plug);
        r.stage("hid");
        r.host.firmware_said(proto::USB_IN, &[0x84, 37, 0, 0, 4, 0, 0, 0, 0, 0]);
        r.host.firmware_said(proto::USB_IN, &[0x84, 193, 2, 1, 255, 1]);
        let valid = r.wait("both report IDs", |u| u["typed"] == "a" && u["mouse"]["buttons"] == json!([2]));
        for payload in
            [&[0x84, 1, 0, 0, 5, 0, 0, 0, 0, 0][..], &[0x84, 193, 0, 1][..], &[0x84, 37, 0, 0, 1, 1, 1, 1, 1, 1][..]]
        {
            r.host.firmware_said(proto::USB_IN, payload);
        }
        let u = r.wait("invalid report diagnostic", |u| {
            u["interfaces"][0]["error"].as_str().is_some_and(|s| s.contains("rollover"))
        });
        for field in ["hid", "keys_down", "modifiers", "mouse", "typed"] {
            assert_eq!(u[field], valid[field], "{field}");
        }
        r.host.firmware_said(proto::USB_IN, &[0x84, 193, 2, 1, 255, 1]);
        let u = r.wait("repeated relative motion", |u| u["hid"]["reports"][1]["mouse"]["total"]["x"] == 2);
        assert_eq!(u["typed"], "a");
        assert_eq!(u["keys_down"], json!([4]));
        r.host.command(Cmd::Unplug);
        let u = r.stage("no_cable");
        assert_eq!(u["hid"]["reports"], json!([]));
        assert_eq!(u["modifiers"], 0);
        assert_eq!(u["mouse"]["buttons"], json!([]));
    }

    #[test]
    fn two_keyboards_with_the_same_report_id_do_not_share_held_keys_or_dead_keys() {
        let report = hid::test_support::numbered(hid::test_support::keyboard(), 61);
        let r = rig(composite_fake(&[(8, 0x82, report.clone()), (2, 0x85, report)]));
        r.host.command(Cmd::Plug);
        r.stage("keyboard");
        for ep in [0x82, 0x85] {
            r.host.firmware_said(proto::USB_IN, &[ep, 61, 0, 0, 4, 0, 0, 0, 0, 0]);
        }
        r.wait("independent presses", |u| u["typed"] == "aa");
        r.host.firmware_said(proto::USB_IN, &[0x82, 61, 0, 0, 0, 0, 0, 0, 0, 0]);
        r.host.firmware_said(proto::USB_IN, &[0x82, 61, 0, 0, 52, 0, 0, 0, 0, 0]);
        r.host.firmware_said(proto::USB_IN, &[0x85, 61, 0, 0, 8, 0, 0, 0, 0, 0]);
        r.host.firmware_said(proto::USB_IN, &[0x82, 61, 0, 0, 8, 0, 0, 0, 0, 0]);
        r.wait("independent dead keys", |u| u["typed"] == "aaeé");
        r.host.rebooted();
        let u = r.wait("new boot enumerated with no held keys", |u| {
            u["stage"] == "keyboard"
                && u["hid"]["reports"] == json!([])
                && u["log"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("restarted"))
        });
        assert_eq!(u["hid"]["reports"], json!([]));
    }

    #[test]
    fn one_invalid_hid_interface_does_not_block_a_supported_interface() {
        let r = rig(composite_fake(&[(5, 0x83, vec![0x05]), (7, 0x86, hid::test_support::mouse())]));
        r.host.command(Cmd::Plug);
        let u = r.stage("mouse");
        assert_eq!(u["hid"]["keyboard"], false);
        assert_eq!(u["hid"]["mouse"], true);
        assert!(u["interfaces"][0]["error"].as_str().unwrap().contains("truncated"));
        r.host.firmware_said(proto::USB_IN, &[0x86, 1, 1, 2, 3]);
        r.wait("supported mouse still works", |u| u["mouse"]["buttons"] == json!([1]));
    }

    fn finish_config(config: &mut [u8], interfaces: u8) {
        let length = (config.len() as u16).to_le_bytes();
        config[2..4].copy_from_slice(&length);
        config[4] = interfaces;
    }

    #[test]
    fn unrelated_usb_interfaces_and_unsupported_msc_do_not_prevent_hid() {
        let mut f = composite_fake(&[(5, 0x83, hid::test_support::mouse())]);
        let cfg = f.configuration.as_mut().unwrap();
        cfg.extend([9, 4, 2, 0, 1, 0xff, 0, 0, 0]);
        cfg.extend([7, 5, 0x87, 3, 64, 0, 1]);
        cfg.extend([9, 4, 1, 0, 0, 8, 6, 0x50, 0]);
        finish_config(cfg, 3);
        let r = rig(f);
        r.host.command(Cmd::Plug);
        let u = r.stage("mouse");
        assert!(u["interfaces"][1]["error"].as_str().unwrap().contains("unsupported USB"));
        assert!(u["interfaces"][2]["error"].as_str().unwrap().contains("MSC"));
        r.host.firmware_said(proto::USB_IN, &[0x87, 0, 0, 4, 0, 0, 0, 0, 0]);
        r.host.firmware_said(proto::USB_IN, &[0x83, 1, 1, 2, 3]);
        let u = r.wait("only declared HID endpoint is decoded", |u| u["mouse"]["buttons"] == json!([1]));
        assert_eq!(u["typed"], "");
    }

    #[test]
    fn a_configuration_beyond_512_bytes_enumerates_its_late_hid_descriptor() {
        let mut f = composite_fake(&[(5, 0x83, hid::test_support::mouse())]);
        let cfg = f.configuration.as_mut().unwrap();
        for _ in 0..8 {
            let mut vendor = vec![0; 100];
            vendor[0] = 100;
            vendor[1] = 0x40;
            cfg.splice(18..18, vendor);
        }
        finish_config(cfg, 1);
        let r = rig(f);
        r.host.command(Cmd::Plug);
        let u = r.stage("mouse");
        assert_eq!(u["interfaces"][0]["report_length"], hid::test_support::mouse().len());
        r.host.firmware_said(proto::USB_IN, &[0x83, 2, 1, 2, 3]);
        r.wait("late HID endpoint", |u| u["mouse"]["buttons"] == json!([2]));
    }

    #[test]
    fn keyboard_and_msc_interfaces_keep_independent_endpoints_and_drivers() {
        let mut f = composite_fake(&[(4, 0x84, hid::test_support::keyboard())]);
        f.disk = volume(4);
        let cfg = f.configuration.as_mut().unwrap();
        cfg.extend_from_slice(&msc_config()[9..]);
        finish_config(cfg, 2);
        let r = rig(f);
        r.host.command(Cmd::Plug);
        let u = r.stage("mounted");
        assert_eq!(u["device"]["class"], "composite");
        assert_eq!(u["hid"]["keyboard"], true);
        r.host.firmware_said(proto::USB_IN, &[0x84, 0, 0, 4, 0, 0, 0, 0, 0]);
        r.host.command(Cmd::List);
        let u = r.wait("disk and typed key", |u| {
            u["typed"] == "a" && u["disk"]["files"].as_array().is_some_and(|f| !f.is_empty())
        });
        assert_eq!(u["disk"]["files"][0]["name"], "hello.txt");
        assert_eq!(u["keys_down"], json!([4]));
    }

    #[test]
    fn endpoint_closure_clears_hid_state_and_old_boot_reports_are_ignored() {
        let (_, rx) = mpsc::channel();
        let mut host = Host::new(rx, Arc::new(|_, _| {}), Arc::new(|_| {}));
        let f = composite_fake(&[(5, 0x83, hid::test_support::keyboard())]);
        host.parse_config(f.configuration.as_ref().unwrap()).unwrap();
        host.hid.insert(
            5,
            HidInterface {
                descriptor: Some(hid::Descriptor::parse(&hid::test_support::keyboard()).unwrap()),
                error: String::new(),
            },
        );
        host.cable = true;
        host.pullup = true;
        host.stage = Stage::Keyboard;
        host.apply(Ev::In(0x83, vec![2, 0, 4, 0, 0, 0, 0, 0]));
        assert_eq!(host.snapshot()["usb"]["keys_down"], json!([4]));
        host.apply(Ev::Dev(DEV_EP_CLOSE, 0x83));
        assert_eq!(host.snapshot()["usb"]["keys_down"], json!([]));
        host.apply(Ev::In(0x83, vec![0, 0, 52, 0, 0, 0, 0, 0]));
        assert!(host.dead.is_empty(), "a closed endpoint cannot produce input");
        host.apply(Ev::Dev(DEV_EP_OPEN, 0x83));
        host.apply(Ev::In(0x83, vec![0, 0, 52, 0, 0, 0, 0, 0]));
        assert!(!host.dead.is_empty(), "a new endpoint can carry reports again");
        host.apply(Ev::Dev(DEV_EP_CLOSE, 0xff));
        host.apply(Ev::In(0x83, vec![0, 0, 8, 0, 0, 0, 0, 0]));
        assert_eq!(host.typed, "A", "closing all endpoints must also ignore stale reports");
        host.apply(Ev::Rebooted);
        host.apply(Ev::In(0x83, vec![0, 0, 8, 0, 0, 0, 0, 0]));
        assert_eq!(host.typed, "A");
        assert!(host.hid_states.is_empty() && host.dead.is_empty());
    }

    #[test]
    fn endpoint_closure_is_published_before_reopened_counters_can_overtake_the_old_state() {
        let (_, rx) = mpsc::channel();
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let output = snapshots.clone();
        let mut host = Host::new(rx, Arc::new(|_, _| {}), Arc::new(move |j| output.lock().unwrap().push(j)));
        let f = composite_fake(&[(5, 0x83, hid::test_support::mouse())]);
        host.parse_config(f.configuration.as_ref().unwrap()).unwrap();
        host.hid.insert(
            5,
            HidInterface {
                descriptor: Some(hid::Descriptor::parse(&hid::test_support::mouse()).unwrap()),
                error: String::new(),
            },
        );
        host.cable = true;
        host.pullup = true;
        host.stage = Stage::Mouse;
        host.apply(Ev::In(0x83, vec![1, 10, 0, 0]));
        host.apply(Ev::Dev(DEV_EP_CLOSE, 0x83));
        {
            let emitted = snapshots.lock().unwrap();
            assert_eq!(emitted.len(), 2);
            assert_eq!(emitted[0]["usb"]["hid"]["reports"][0]["mouse"]["sequence"], 1);
            assert_eq!(emitted[1]["usb"]["hid"]["reports"], json!([]));
            assert_eq!(emitted[1]["usb"]["mouse"]["buttons"], json!([]));
        }
        host.apply(Ev::Dev(DEV_EP_OPEN, 0x83));
        for _ in 0..3 {
            host.apply(Ev::In(0x83, vec![1, 10, 0, 0]));
        }
        host.publish(true);
        let emitted = snapshots.lock().unwrap();
        assert_eq!(emitted.last().unwrap()["usb"]["hid"]["reports"][0]["mouse"]["sequence"], 3);
        assert_eq!(emitted.last().unwrap()["usb"]["hid"]["reports"][0]["mouse"]["total"]["x"], 30);
    }

    #[test]
    fn closing_the_mouse_endpoint_preserves_the_keyboards_pending_accent() {
        let (_, rx) = mpsc::channel();
        let mut host = Host::new(rx, Arc::new(|_, _| {}), Arc::new(|_| {}));
        let keyboard = hid::test_support::keyboard();
        let mouse = hid::test_support::mouse();
        let f = composite_fake(&[(5, 0x83, keyboard.clone()), (7, 0x86, mouse.clone())]);
        host.parse_config(f.configuration.as_ref().unwrap()).unwrap();
        for (number, report) in [(5, keyboard), (7, mouse)] {
            host.hid.insert(
                number,
                HidInterface { descriptor: Some(hid::Descriptor::parse(&report).unwrap()), error: String::new() },
            );
        }
        host.cable = true;
        host.pullup = true;
        host.stage = Stage::Hid;
        host.apply(Ev::In(0x83, vec![0, 0, 52, 0, 0, 0, 0, 0]));
        host.apply(Ev::In(0x86, vec![1, 1, 2, 3]));
        assert_eq!(host.dead.len(), 1);
        host.apply(Ev::Dev(DEV_EP_CLOSE, 0x86));
        assert_eq!(host.snapshot()["usb"]["keys_down"], json!([52]));
        assert_eq!(host.snapshot()["usb"]["mouse"]["buttons"], json!([]));
        host.apply(Ev::In(0x83, vec![0, 0, 8, 0, 0, 0, 0, 0]));
        assert_eq!(host.typed, "é");
        assert!(host.dead.is_empty());
    }

    #[test]
    fn invalid_second_collection_does_not_commit_the_first_collection() {
        let mut descriptor = hid::test_support::keyboard();
        descriptor.extend(hid::test_support::mouse());
        let r = rig(composite_fake(&[(3, 0x85, descriptor)]));
        r.host.command(Cmd::Plug);
        r.stage("hid");
        r.host.firmware_said(proto::USB_IN, &[0x85, 0, 0, 4, 0, 0, 0, 0, 0, 1, 1, 2, 3]);
        let valid = r.wait("valid two-collection report", |u| u["typed"] == "a" && u["mouse"]["buttons"] == json!([1]));
        r.host.firmware_said(proto::USB_IN, &[0x85, 0, 0, 5, 0, 0, 0, 0, 0, 0, 128, 2, 3]);
        let invalid = r.wait("invalid mouse in combined report", |u| {
            u["interfaces"][0]["error"].as_str().is_some_and(|s| s.contains("logical range"))
        });
        for field in ["hid", "keys_down", "modifiers", "mouse", "typed"] {
            assert_eq!(invalid[field], valid[field]);
        }
    }

    #[test]
    fn two_keyboard_collections_in_one_interface_keep_their_own_modifiers() {
        let mut descriptor = hid::test_support::numbered(hid::test_support::keyboard(), 47);
        descriptor.extend(hid::test_support::numbered(hid::test_support::keyboard(), 89));
        let r = rig(composite_fake(&[(3, 0x85, descriptor)]));
        r.host.command(Cmd::Plug);
        r.stage("keyboard");
        r.host.firmware_said(proto::USB_IN, &[0x85, 47, 2, 0, 4, 0, 0, 0, 0, 0]);
        r.host.firmware_said(proto::USB_IN, &[0x85, 89, 0, 0, 4, 0, 0, 0, 0, 0]);
        r.wait("collection-specific modifiers", |u| u["typed"] == "Aa");
        r.host.firmware_said(proto::USB_IN, &[0x85, 47, 0, 0, 0, 0, 0, 0, 0, 0]);
        let u = r.wait("first collection released, second still held", |u| {
            u["modifiers"] == 0 && u["keys_down"] == json!([4])
        });
        assert_eq!(u["hid"]["reports"][1]["keys_down"], json!([4]));
    }

    #[test]
    fn boot_capability_requires_a_valid_descriptor_and_successful_report_protocol_selection() {
        for protocol_stall in [false, true] {
            let mut f = Fake::new(vec![]);
            f.keyboard = true;
            f.configuration = Some(kbd_config());
            f.protocol_stall = protocol_stall;
            f.reports.insert(0, if protocol_stall { hid::test_support::keyboard() } else { vec![0x05] });
            if !protocol_stall {
                let cfg = f.configuration.as_mut().unwrap();
                let report_len = f.reports[&0].len() as u16;
                cfg[25..27].copy_from_slice(&report_len.to_le_bytes());
            }
            let r = rig(f);
            r.host.command(Cmd::Plug);
            let u = r.stage("failed");
            let error = u["interfaces"][0]["error"].as_str().unwrap();
            assert!(error.contains(if protocol_stall { "SET_PROTOCOL Report" } else { "truncated" }));
            assert_eq!(u["hid"]["keyboard"], false);
            r.host.firmware_said(proto::USB_IN, &[0x81, 0, 0, 4, 0, 0, 0, 0, 0]);
            r.host.command(Cmd::ClearTyped);
            r.wait("invalid interface never decodes boot bytes", |u| u["typed"] == "" && u["keys_down"] == json!([]));
        }
    }

    #[test]
    fn quick_click_edges_survive_thinning_and_overflow_preserves_all_state() {
        let (_, rx) = mpsc::channel();
        let mut host = Host::new(rx, Arc::new(|_, _| {}), Arc::new(|_| {}));
        let f = composite_fake(&[(5, 0x83, hid::test_support::mouse())]);
        host.parse_config(f.configuration.as_ref().unwrap()).unwrap();
        host.hid.insert(
            5,
            HidInterface {
                descriptor: Some(hid::Descriptor::parse(&hid::test_support::mouse()).unwrap()),
                error: String::new(),
            },
        );
        host.cable = true;
        host.pullup = true;
        host.stage = Stage::Mouse;
        host.apply(Ev::In(0x83, vec![5, 0, 0, 0]));
        host.apply(Ev::In(0x83, vec![0, 0, 0, 0]));
        let u = host.snapshot()["usb"].clone();
        assert_eq!(u["mouse"]["buttons"], json!([]));
        assert_eq!(u["hid"]["reports"][0]["mouse"]["presses"], json!({"1":1,"3":1}));
        assert_eq!(u["hid"]["reports"][0]["mouse"]["releases"], json!({"1":1,"3":1}));
        assert_eq!(u["hid"]["reports"][0]["mouse"]["sequence"], 2);
        host.hid_states.values_mut().next().unwrap().totals.insert(hid::USAGE_X, i64::MAX);
        let before = host.hid_states.clone();
        host.report(5, 0x83, &[1, 1, 0, 0]);
        assert!(host.hid_states == before);
        assert!(host.hid[&5].error.contains("overflow"));
        host.hid_states.values_mut().next().unwrap().presses.insert(2, u64::MAX);
        let before = host.hid_states.clone();
        host.report(5, 0x83, &[2, 0, 0, 0]);
        assert!(host.hid_states == before);
        assert!(host.hid[&5].error.contains("counter overflow"));
    }

    #[test]
    fn keys_are_typed_once_while_held_and_backspace_erases() {
        let mut f = Fake::new(vec![]);
        f.keyboard = true;
        let r = rig(f);
        r.host.command(Cmd::Plug);
        r.stage("keyboard");
        for _ in 0..3 {
            r.host.firmware_said(proto::USB_IN, &[0x81, 0, 0, 4, 0, 0, 0, 0, 0]);
        }
        r.host.firmware_said(proto::USB_IN, &[0x81, 0, 0, 4, 5, 0, 0, 0, 0]);
        r.host.firmware_said(proto::USB_IN, &[0x81, 0, 0, 0, 0, 0, 0, 0, 0]);
        r.host.firmware_said(proto::USB_IN, &[0x81, 0, 0, 0x2A, 0, 0, 0, 0, 0]);
        r.wait("the text", |u| u["typed"] == "a");
    }

    #[test]
    fn the_partition_table_is_followed_and_a_bare_volume_is_not_mistaken_for_one() {
        let mut mbr = vec![0u8; 512];
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        mbr[446 + 4] = 0x0C;
        mbr[446 + 8..446 + 12].copy_from_slice(&2048u32.to_le_bytes());
        assert_eq!(partition_start(&mbr), 2048);
        let vol = volume(1);
        assert_eq!(partition_start(&vol[..512]), 0);
        assert_eq!(partition_start(&vec![0u8; 512]), 0);
    }

    #[test]
    fn file_names_are_one_name_in_the_root() {
        assert_eq!(clean_name("../../etc/passwd"), "etcpasswd");
        assert_eq!(clean_name("a:b*c?.txt"), "abc.txt");
        assert_eq!(clean_name("..."), "kode-sim.bin");
        assert_eq!(clean_name("Foto de Víctor.png"), "Foto de Víctor.png");
    }

    #[test]
    fn commands_come_from_the_page_s_json() {
        let c = Cmd::from_json(&json!({"cmd": "put", "name": "a.txt", "data": "aGk="})).unwrap();
        assert!(matches!(c, Cmd::Put { ref name, ref data } if name == "a.txt" && data == b"hi"));
        assert!(matches!(Cmd::from_json(&json!({"cmd": "eject"})), Some(Cmd::Eject)));
        assert!(Cmd::from_json(&json!({"cmd": "format_everything"})).is_none());
        assert!(Cmd::from_json(&json!({"cmd": "put", "name": "a", "data": "!!"})).is_none());
    }

    #[test]
    fn the_crc_is_the_usual_one() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}

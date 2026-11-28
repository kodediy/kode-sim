//! The USB host's frames, carried over esp-emu's USB host over
//! co-simulation (esp-emu 1.3.0's `UsbHostClaim`, `UsbTransfer`,
//! `UsbResult` and `UsbLink`).
//!
//! The host ([`crate::usbhost`]) speaks the frames of a device controller
//! that tells it every packet: a SETUP and the stages of a control
//! transfer, the data the device queues on an IN endpoint as it queues it,
//! credits back for what an OUT endpoint took, its pull-up and its stalls.
//! esp-emu's host runs whole transfers instead, on the chip's real USB-OTG
//! controller and the firmware's own TinyUSB. This is the translation, a
//! state machine with no I/O: [`Link::host_said`] takes a frame the host
//! sends, [`Link::emu_said`] what the emulator says, and each gives back
//! what to do - transfers to start, frames for the host.
//!
//! - A control transfer goes out as one transfer, once its SETUP (and its
//!   OUT data, when it has some) is in. Its IN data comes back cut into
//!   packets of endpoint 0's size, the way the host reads a data stage, and
//!   its status stage as an empty frame.
//! - The IN endpoints of the configuration the host set are read all the
//!   time, one transfer at a time each, as a computer polls them: what a
//!   transfer brings is what the device queued. A mass-storage command's
//!   status, which may end the same transfer as its data, is handed over as
//!   a frame of its own.
//! - An OUT transfer's end is the credit back.
//! - The link is the pull-up: a device attached, or not.

use std::collections::{BTreeSet, HashMap};

use cosim::{Payload, UsbStatus, UsbTransfer};

use crate::proto;
use crate::usbhost::{BUS_CABLE_IN, BUS_CABLE_OUT, DEV_PULLUP, DEV_STALL};

/// What one IN transfer on a bulk endpoint asks for at most: a whole number
/// of packets at either speed.
const BULK_IN: u32 = 16 * 1024;
/// How long a control transfer may wait on the firmware, in emulated µs.
const CONTROL_TIMEOUT_US: u32 = 5_000_000;

/// What to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Do {
    /// Start this transfer on the emulator.
    Transfer(UsbTransfer),
    /// Hand the host this frame, as if the device controller had sent it.
    Host(u8, Vec<u8>),
}

#[derive(Clone, Debug)]
enum Waiting {
    Control { setup: [u8; 8] },
    Out { ep: u8 },
    In { ep: u8 },
}

#[derive(Clone, Copy, Debug)]
struct InEndpoint {
    address: u8,
    interrupt: bool,
    mps: u16,
}

/// The translation's state.
pub struct Link {
    next_id: u64,
    waiting: HashMap<u64, Waiting>,
    /// The device is there (esp-emu's UsbLink).
    attached: bool,
    /// The host has its cable in.
    cable: bool,
    /// A control write's SETUP, waiting for its OUT data.
    setup_out: Option<[u8; 8]>,
    ep0_mps: usize,
    /// The IN endpoints of the configuration last read, by configuration.
    config_ins: HashMap<u8, Vec<InEndpoint>>,
    /// The IN endpoints being read now, the configuration set.
    polled: Vec<InEndpoint>,
    /// IN endpoints that stalled: not read again until the host clears them.
    halted: BTreeSet<u8>,
    /// IN endpoints with a transfer out.
    reading: BTreeSet<u8>,
}

impl Default for Link {
    fn default() -> Self {
        Self::new()
    }
}

impl Link {
    pub fn new() -> Link {
        Link {
            next_id: 1,
            waiting: HashMap::new(),
            attached: false,
            cable: false,
            setup_out: None,
            ep0_mps: 64,
            config_ins: HashMap::new(),
            polled: vec![],
            halted: BTreeSet::new(),
            reading: BTreeSet::new(),
        }
    }

    /// A frame the host sends the device controller.
    pub fn host_said(&mut self, t: u8, p: &[u8]) -> Vec<Do> {
        match t {
            proto::USB_BUS if p.first() == Some(&BUS_CABLE_IN) => {
                self.cable = true;
                if self.attached {
                    return vec![pullup(true)];
                }
                vec![]
            }
            proto::USB_BUS if p.first() == Some(&BUS_CABLE_OUT) => {
                // The device does not see the cable go: its endpoints keep
                // what the host had asked for. A read still out stays out,
                // and serves the next attachment's host.
                self.cable = false;
                self.setup_out = None;
                self.polled.clear();
                self.halted.clear();
                self.waiting.retain(|_, w| matches!(w, Waiting::In { .. }));
                vec![pullup(false)]
            }
            proto::USB_SETUP if p.len() == 8 => {
                let mut setup = [0u8; 8];
                setup.copy_from_slice(p);
                let w_length = u16::from_le_bytes([setup[6], setup[7]]);
                if setup[0] & 0x80 == 0 && w_length > 0 {
                    self.setup_out = Some(setup);
                    return vec![];
                }
                self.setup_out = None;
                vec![self.control(setup, vec![])]
            }
            proto::USB_OUT if !p.is_empty() => {
                let ep = p[0] & 0x7f;
                if ep == 0 {
                    // A control write's data, or a control read's status
                    // stage, which esp-emu ran with the transfer.
                    match self.setup_out.take() {
                        Some(setup) => vec![self.control(setup, p[1..].to_vec())],
                        None => vec![],
                    }
                } else {
                    let id = self.id(Waiting::Out { ep });
                    vec![Do::Transfer(UsbTransfer {
                        id,
                        endpoint: ep,
                        setup: vec![],
                        data: p[1..].to_vec(),
                        length: 0,
                        timeout_us: 0,
                    })]
                }
            }
            _ => vec![],
        }
    }

    /// What the emulator said: its UsbLink and UsbResult; nothing else.
    pub fn emu_said(&mut self, payload: &Payload) -> Vec<Do> {
        match payload {
            Payload::UsbLink { attached, .. } => {
                let was = self.attached;
                self.attached = *attached;
                self.forget();
                if *attached == was {
                    return vec![];
                }
                if *attached && self.cable {
                    vec![pullup(true)]
                } else if !*attached {
                    vec![pullup(false)]
                } else {
                    vec![]
                }
            }
            Payload::UsbResult { id, status, data, sent } => match self.waiting.remove(id) {
                None => vec![],
                Some(Waiting::Control { setup }) => self.control_ended(setup, *status, data),
                Some(Waiting::Out { ep }) => match status {
                    UsbStatus::Ok => {
                        let mut f = vec![ep];
                        f.extend_from_slice(&sent.to_le_bytes());
                        vec![Do::Host(proto::USB_ACK, f)]
                    }
                    UsbStatus::Stall => vec![Do::Host(proto::USB_DEV, vec![DEV_STALL, ep, 0, 0])],
                    _ => vec![],
                },
                Some(Waiting::In { ep }) => {
                    self.reading.remove(&ep);
                    let mut out = vec![];
                    if !self.polled.iter().any(|e| e.address == ep) {
                        return out; // read for a host that has since let go
                    }
                    match status {
                        UsbStatus::Ok => out.extend(in_frames(ep, data)),
                        UsbStatus::Stall => {
                            self.halted.insert(ep);
                            out.push(Do::Host(proto::USB_DEV, vec![DEV_STALL, ep, 0, 0]));
                        }
                        UsbStatus::NoDevice | UsbStatus::Refused => return out,
                        _ => {}
                    }
                    out.extend(self.read(ep));
                    out
                }
            },
            _ => vec![],
        }
    }

    fn id(&mut self, w: Waiting) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.waiting.insert(id, w);
        id
    }

    /// What was under way went with the device or the cable.
    fn forget(&mut self) {
        self.waiting.clear();
        self.setup_out = None;
        self.polled.clear();
        self.halted.clear();
        self.reading.clear();
    }

    fn control(&mut self, setup: [u8; 8], data: Vec<u8>) -> Do {
        let id = self.id(Waiting::Control { setup });
        Do::Transfer(UsbTransfer {
            id,
            endpoint: 0,
            setup: setup.to_vec(),
            data,
            length: 0,
            timeout_us: CONTROL_TIMEOUT_US,
        })
    }

    fn control_ended(&mut self, setup: [u8; 8], status: UsbStatus, data: &[u8]) -> Vec<Do> {
        match status {
            UsbStatus::Ok => {}
            UsbStatus::Stall => return vec![Do::Host(proto::USB_DEV, vec![DEV_STALL, 0x80, 0, 0])],
            _ => return vec![],
        }
        let (kind, request) = (setup[0], setup[1]);
        let value = u16::from_le_bytes([setup[2], setup[3]]);
        let mut out = vec![];
        if kind & 0x80 != 0 {
            if (kind, request, value >> 8) == (0x80, 6, 1) && data.len() >= 8 {
                self.ep0_mps = usize::from(data[7]).max(8);
            }
            if (kind, request, value >> 8) == (0x80, 6, 2) {
                self.note_config(data);
            }
            let w_length = usize::from(u16::from_le_bytes([setup[6], setup[7]]));
            for packet in data.chunks(self.ep0_mps) {
                out.push(Do::Host(proto::USB_IN, [&[0x80][..], packet].concat()));
            }
            if data.len().is_multiple_of(self.ep0_mps) && data.len() < w_length {
                out.push(Do::Host(proto::USB_IN, vec![0x80]));
            }
        } else {
            // The status stage, an IN of nothing.
            out.push(Do::Host(proto::USB_IN, vec![0x80]));
            match (kind, request) {
                // SET_CONFIGURATION: its IN endpoints are read from now on.
                (0x00, 9) => {
                    self.polled = self.config_ins.get(&(value as u8)).cloned().unwrap_or_default();
                    self.halted.clear();
                    let eps: Vec<u8> = self.polled.iter().map(|e| e.address).collect();
                    for ep in eps {
                        out.extend(self.read(ep));
                    }
                }
                // CLEAR_FEATURE(ENDPOINT_HALT): read it again.
                (0x02, 1) if value == 0 => {
                    let ep = setup[4];
                    if self.halted.remove(&ep) {
                        out.extend(self.read(ep));
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// The IN endpoints of a configuration descriptor read whole.
    fn note_config(&mut self, config: &[u8]) {
        if config.len() < 9 || config.len() < usize::from(u16::from_le_bytes([config[2], config[3]])) {
            return;
        }
        let mut ins = vec![];
        let (mut i, mut alternate) = (0usize, 0u8);
        while i + 2 <= config.len() && config[i] >= 2 {
            let d = &config[i..(i + usize::from(config[i])).min(config.len())];
            match d.get(1) {
                Some(4) if d.len() >= 4 => alternate = d[3],
                Some(5) if d.len() >= 7 && alternate == 0 && d[2] & 0x80 != 0 => {
                    let kind = d[3] & 3;
                    if kind == 2 || kind == 3 {
                        ins.push(InEndpoint {
                            address: d[2],
                            interrupt: kind == 3,
                            mps: u16::from_le_bytes([d[4], d[5]]) & 0x7ff,
                        });
                    }
                }
                _ => {}
            }
            i += usize::from(config[i]);
        }
        self.config_ins.insert(config[5], ins);
    }

    /// One more read of IN endpoint `ep`, if it is polled and has none out.
    fn read(&mut self, ep: u8) -> Vec<Do> {
        let Some(e) = self.polled.iter().find(|e| e.address == ep).copied() else { return vec![] };
        if !self.attached || self.halted.contains(&ep) || !self.reading.insert(ep) {
            return vec![];
        }
        let length = if e.interrupt { u32::from(e.mps.max(1)) } else { BULK_IN };
        let id = self.id(Waiting::In { ep });
        vec![Do::Transfer(UsbTransfer { id, endpoint: ep, setup: vec![], data: vec![], length, timeout_us: 0 })]
    }
}

fn pullup(up: bool) -> Do {
    Do::Host(proto::USB_DEV, vec![DEV_PULLUP, up as u8, 0, 0])
}

/// What one IN transfer brought, as the frames of a controller that hands
/// each queued piece on: a bulk-only command's 13-byte status, when it
/// ended the same transfer as the command's data, as a frame of its own.
fn in_frames(ep: u8, data: &[u8]) -> Vec<Do> {
    if data.is_empty() {
        return vec![];
    }
    let frame = |d: &[u8]| Do::Host(proto::USB_IN, [&[ep][..], d].concat());
    let n = data.len();
    if n > 13 && &data[n - 13..n - 9] == b"USBS" {
        return vec![frame(&data[..n - 13]), frame(&data[n - 13..])];
    }
    vec![frame(data)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer(d: &[Do]) -> UsbTransfer {
        match d {
            [Do::Transfer(t)] => t.clone(),
            other => panic!("not one transfer: {other:?}"),
        }
    }

    fn result(id: u64, status: UsbStatus, data: &[u8], sent: u32) -> Payload {
        Payload::UsbResult { id, status, data: data.to_vec(), sent }
    }

    fn host(t: u8, p: &[u8]) -> Do {
        Do::Host(t, p.to_vec())
    }

    /// A link with the cable in and a device there.
    fn plugged() -> Link {
        let mut l = Link::new();
        assert_eq!(l.host_said(proto::USB_BUS, &[BUS_CABLE_IN]), vec![]);
        assert_eq!(
            l.emu_said(&Payload::UsbLink { attached: true, high_speed: false }),
            vec![host(proto::USB_DEV, &[DEV_PULLUP, 1, 0, 0])]
        );
        l
    }

    #[test]
    fn the_pull_up_is_the_device_there_with_the_cable_in() {
        let mut l = Link::new();
        // A device with no cable in: the host hears nothing yet.
        assert_eq!(l.emu_said(&Payload::UsbLink { attached: true, high_speed: false }), vec![]);
        assert_eq!(l.host_said(proto::USB_BUS, &[BUS_CABLE_IN]), vec![host(proto::USB_DEV, &[DEV_PULLUP, 1, 0, 0])]);
        assert_eq!(
            l.emu_said(&Payload::UsbLink { attached: false, high_speed: false }),
            vec![host(proto::USB_DEV, &[DEV_PULLUP, 0, 0, 0])]
        );
        assert_eq!(l.host_said(proto::USB_BUS, &[BUS_CABLE_OUT]), vec![host(proto::USB_DEV, &[DEV_PULLUP, 0, 0, 0])]);
    }

    #[test]
    fn a_control_read_comes_back_in_packets_of_endpoint_zero() {
        let mut l = plugged();
        let get_device = [0x80, 6, 0, 1, 0, 0, 18, 0];
        let t = transfer(&l.host_said(proto::USB_SETUP, &get_device));
        assert_eq!((t.endpoint, t.setup.as_slice()), (0, &get_device[..]));
        // Endpoint 0 is 8 bytes on this device: 18 bytes are three packets.
        let mut dev = vec![18, 1, 0, 2, 0, 0, 0, 8];
        dev.extend_from_slice(&[0; 10]);
        let frames = l.emu_said(&result(t.id, UsbStatus::Ok, &dev, 0));
        let sizes: Vec<usize> = frames
            .iter()
            .map(|d| match d {
                Do::Host(proto::USB_IN, f) if f[0] == 0x80 => f.len() - 1,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(sizes, vec![8, 8, 2]);
        // The host's status OUT was the transfer's: nothing more goes out.
        assert_eq!(l.host_said(proto::USB_OUT, &[0]), vec![]);
    }

    #[test]
    fn a_read_that_ends_on_a_whole_packet_short_of_its_length_is_closed_by_an_empty_one() {
        let mut l = plugged();
        let t = transfer(&l.host_said(proto::USB_SETUP, &[0x80, 6, 0, 2, 0, 0, 255, 0]));
        let frames = l.emu_said(&result(t.id, UsbStatus::Ok, &[9; 64], 0));
        assert_eq!(frames.last(), Some(&host(proto::USB_IN, &[0x80])));
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn a_control_write_waits_for_its_data_and_ends_with_an_empty_status() {
        let mut l = plugged();
        let set_report = [0x21, 9, 0, 2, 0, 0, 1, 0];
        assert_eq!(l.host_said(proto::USB_SETUP, &set_report), vec![]);
        let t = transfer(&l.host_said(proto::USB_OUT, &[0, 0x02]));
        assert_eq!((t.setup.as_slice(), t.data.as_slice()), (&set_report[..], &[0x02][..]));
        assert_eq!(l.emu_said(&result(t.id, UsbStatus::Ok, &[], 1)), vec![host(proto::USB_IN, &[0x80])]);
    }

    #[test]
    fn a_stalled_request_is_a_stall_on_endpoint_zero() {
        let mut l = plugged();
        let t = transfer(&l.host_said(proto::USB_SETUP, &[0x21, 0x0b, 0, 0, 0, 0, 0, 0]));
        assert_eq!(
            l.emu_said(&result(t.id, UsbStatus::Stall, &[], 0)),
            vec![host(proto::USB_DEV, &[DEV_STALL, 0x80, 0, 0])]
        );
    }

    /// A configuration with a boot keyboard (interrupt IN 0x81, 8 bytes)
    /// and a mass-storage interface (bulk OUT 0x02, bulk IN 0x82), and an
    /// alternate setting whose endpoint is not read.
    fn config() -> Vec<u8> {
        let mut c = vec![9, 2, 0, 0, 2, 1, 0, 0x80, 50];
        c.extend_from_slice(&[9, 4, 0, 0, 1, 3, 1, 1, 0]);
        c.extend_from_slice(&[9, 0x21, 0x11, 1, 0, 1, 0x22, 63, 0]);
        c.extend_from_slice(&[7, 5, 0x81, 3, 8, 0, 10]);
        c.extend_from_slice(&[9, 4, 1, 0, 2, 8, 6, 0x50, 0]);
        c.extend_from_slice(&[7, 5, 0x02, 2, 64, 0, 0]);
        c.extend_from_slice(&[7, 5, 0x82, 2, 64, 0, 0]);
        c.extend_from_slice(&[9, 4, 1, 1, 1, 8, 6, 0x50, 0]);
        c.extend_from_slice(&[7, 5, 0x83, 2, 64, 0, 0]);
        let n = c.len() as u16;
        c[2..4].copy_from_slice(&n.to_le_bytes());
        c
    }

    /// `plugged`, configured: the transfers SET_CONFIGURATION started.
    fn configured() -> (Link, Vec<UsbTransfer>) {
        let mut l = plugged();
        let c = config();
        let t = transfer(&l.host_said(proto::USB_SETUP, &[0x80, 6, 0, 2, 0, 0, c.len() as u8, 0]));
        l.emu_said(&result(t.id, UsbStatus::Ok, &c, 0));
        let t = transfer(&l.host_said(proto::USB_SETUP, &[0, 9, 1, 0, 0, 0, 0, 0]));
        let mut reads = vec![];
        for d in l.emu_said(&result(t.id, UsbStatus::Ok, &[], 0)) {
            match d {
                Do::Transfer(t) => reads.push(t),
                Do::Host(t, f) => assert_eq!((t, f), (proto::USB_IN, vec![0x80])),
            }
        }
        (l, reads)
    }

    #[test]
    fn the_configuration_s_in_endpoints_are_read_from_set_configuration_on() {
        let (_, reads) = configured();
        let eps: Vec<(u8, u32)> = reads.iter().map(|t| (t.endpoint, t.length)).collect();
        assert_eq!(eps, vec![(0x81, 8), (0x82, BULK_IN)], "the alternate setting's 0x83 is not read");
    }

    #[test]
    fn a_report_is_handed_on_and_the_endpoint_read_again() {
        let (mut l, reads) = configured();
        let kbd = reads[0].clone();
        let report = [0, 0, 4, 0, 0, 0, 0, 0];
        let out = l.emu_said(&result(kbd.id, UsbStatus::Ok, &report, 0));
        assert_eq!(out[0], host(proto::USB_IN, &[&[0x81][..], &report[..]].concat()));
        match &out[1] {
            Do::Transfer(t) => assert_eq!((t.endpoint, t.length), (0x81, 8)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_command_s_status_after_its_data_is_a_frame_of_its_own() {
        let (mut l, reads) = configured();
        let bulk = reads[1].clone();
        let mut data = vec![0xAB; 512];
        let mut csw = b"USBS".to_vec();
        csw.extend_from_slice(&[7, 0, 0, 0, 0, 0, 0, 0, 0]);
        data.extend_from_slice(&csw);
        let out = l.emu_said(&result(bulk.id, UsbStatus::Ok, &data, 0));
        assert_eq!(out[0], host(proto::USB_IN, &[&[0x82][..], &[0xAB; 512][..]].concat()));
        assert_eq!(out[1], host(proto::USB_IN, &[&[0x82][..], &csw[..]].concat()));
        assert!(matches!(&out[2], Do::Transfer(t) if t.endpoint == 0x82));
    }

    #[test]
    fn an_out_transfer_s_end_is_its_credit_back() {
        let (mut l, _) = configured();
        let t = transfer(&l.host_said(proto::USB_OUT, &[0x02, 1, 2, 3]));
        assert_eq!((t.endpoint, t.data.as_slice()), (0x02, &[1, 2, 3][..]));
        assert_eq!(l.emu_said(&result(t.id, UsbStatus::Ok, &[], 3)), vec![host(proto::USB_ACK, &[0x02, 3, 0, 0, 0])]);
    }

    #[test]
    fn a_stalled_in_endpoint_is_read_again_once_the_host_clears_it() {
        let (mut l, reads) = configured();
        let bulk = reads[1].clone();
        assert_eq!(
            l.emu_said(&result(bulk.id, UsbStatus::Stall, &[], 0)),
            vec![host(proto::USB_DEV, &[DEV_STALL, 0x82, 0, 0])]
        );
        let clear = transfer(&l.host_said(proto::USB_SETUP, &[0x02, 1, 0, 0, 0x82, 0, 0, 0]));
        let out = l.emu_said(&result(clear.id, UsbStatus::Ok, &[], 0));
        assert_eq!(out[0], host(proto::USB_IN, &[0x80]));
        assert!(matches!(&out[1], Do::Transfer(t) if t.endpoint == 0x82));
    }

    #[test]
    fn a_read_still_out_when_the_cable_comes_out_serves_the_next_host() {
        let (mut l, reads) = configured();
        assert_eq!(l.host_said(proto::USB_BUS, &[BUS_CABLE_OUT]), vec![host(proto::USB_DEV, &[DEV_PULLUP, 0, 0, 0])]);
        assert_eq!(l.host_said(proto::USB_BUS, &[BUS_CABLE_IN]), vec![host(proto::USB_DEV, &[DEV_PULLUP, 1, 0, 0])]);
        let c = config();
        let t = transfer(&l.host_said(proto::USB_SETUP, &[0x80, 6, 0, 2, 0, 0, c.len() as u8, 0]));
        l.emu_said(&result(t.id, UsbStatus::Ok, &c, 0));
        let t = transfer(&l.host_said(proto::USB_SETUP, &[0, 9, 1, 0, 0, 0, 0, 0]));
        // The reads from before the cable came out are still out: none again.
        assert_eq!(l.emu_said(&result(t.id, UsbStatus::Ok, &[], 0)), vec![host(proto::USB_IN, &[0x80])]);
        let report = [0, 0, 5, 0, 0, 0, 0, 0];
        let out = l.emu_said(&result(reads[0].id, UsbStatus::Ok, &report, 0));
        assert_eq!(out[0], host(proto::USB_IN, &[&[0x81][..], &report[..]].concat()));
    }

    #[test]
    fn what_a_read_brings_with_no_host_on_the_cable_is_dropped() {
        let (mut l, reads) = configured();
        l.host_said(proto::USB_BUS, &[BUS_CABLE_OUT]);
        assert_eq!(l.emu_said(&result(reads[0].id, UsbStatus::Ok, &[0, 0, 4, 0, 0, 0, 0, 0], 0)), vec![]);
    }

    #[test]
    fn a_device_that_leaves_takes_its_transfers_with_it() {
        let (mut l, reads) = configured();
        assert_eq!(
            l.emu_said(&Payload::UsbLink { attached: false, high_speed: false }),
            vec![host(proto::USB_DEV, &[DEV_PULLUP, 0, 0, 0])]
        );
        // The results of the old attachment are nobody's.
        assert_eq!(l.emu_said(&result(reads[0].id, UsbStatus::NoDevice, &[], 0)), vec![]);
        assert_eq!(l.emu_said(&result(reads[0].id, UsbStatus::Ok, &[1, 2], 0)), vec![]);
    }
}

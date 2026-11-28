//! USB 2.0 configuration descriptors, with endpoints owned by an alternate
//! interface setting rather than by their position in the configuration.

use std::collections::BTreeSet;

pub const CLASS_HID: u8 = 0x03;
pub const CLASS_MSC: u8 = 0x08;
pub const TRANSFER_BULK: u8 = 2;
pub const TRANSFER_INTERRUPT: u8 = 3;
pub const DESC_HID: u8 = 0x21;
pub const DESC_REPORT: u8 = 0x22;
/// kemu_usbdcd.h: KEMU_USB_EPS includes control endpoint zero.
pub const CONTROLLER_ENDPOINTS: u8 = 8;

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub address: u8,
    pub attributes: u8,
    pub transactions: u8,
}

impl Endpoint {
    pub fn input(&self) -> bool {
        self.address & 0x80 != 0
    }
    pub fn transfer(&self) -> u8 {
        self.attributes & 3
    }

    pub fn representable(&self) -> bool {
        self.address & 0x0f < CONTROLLER_ENDPOINTS && self.transactions == 1
    }
}

#[derive(Debug, Clone)]
pub struct Interface {
    pub number: u8,
    pub alternate: u8,
    pub class: (u8, u8, u8),
    pub endpoints: Vec<Endpoint>,
    pub report_length: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct Configuration {
    pub value: u8,
    pub interfaces: Vec<Interface>,
}

impl Configuration {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 9 || bytes[0] != 9 || bytes[1] != 2 {
            return Err("invalid USB configuration header".into());
        }
        if usize::from(u16::from_le_bytes([bytes[2], bytes[3]])) != bytes.len() {
            return Err("USB configuration does not match wTotalLength".into());
        }
        if bytes[5] == 0 {
            return Err("USB configuration value zero is reserved".into());
        }
        let mut config = Self { value: bytes[5], interfaces: vec![] };
        let mut expected_endpoints = 0;
        let mut at = 9;
        while at < bytes.len() {
            let header = bytes.get(at..at + 2).ok_or("truncated USB descriptor header")?;
            let len = usize::from(header[0]);
            if len < 2 {
                return Err("USB descriptor length is less than two".into());
            }
            let d = bytes.get(at..at + len).ok_or("truncated USB descriptor")?;
            match d[1] {
                2 => return Err("nested USB configuration descriptor".into()),
                4 => {
                    if len != 9 {
                        return Err("invalid USB interface descriptor length".into());
                    }
                    if config.interfaces.last().is_some_and(|i| i.endpoints.len() != expected_endpoints) {
                        return Err("USB interface endpoint count mismatch".into());
                    }
                    if config.interfaces.iter().any(|i| i.number == d[2] && i.alternate == d[3]) {
                        return Err("duplicate USB interface alternate setting".into());
                    }
                    expected_endpoints = usize::from(d[4]);
                    config.interfaces.push(Interface {
                        number: d[2],
                        alternate: d[3],
                        class: (d[5], d[6], d[7]),
                        endpoints: vec![],
                        report_length: None,
                    });
                }
                5 => {
                    if len < 7 {
                        return Err("short USB endpoint descriptor".into());
                    }
                    let interface = config.interfaces.last_mut().ok_or("USB endpoint without interface")?;
                    let address = d[2];
                    let packet = u16::from_le_bytes([d[4], d[5]]);
                    if address & 0x70 != 0
                        || address & 0x0f == 0
                        || packet & 0xe000 != 0
                        || packet & 0x07ff == 0
                        || packet & 0x1800 == 0x1800
                        || (d[3] & 3 < 3 && d[3] & 3 != 1 && packet & 0x1800 != 0)
                    {
                        return Err("invalid USB endpoint address or packet size".into());
                    }
                    if interface.endpoints.iter().any(|e| e.address == address) {
                        return Err("duplicate endpoint in USB interface".into());
                    }
                    interface.endpoints.push(Endpoint {
                        address,
                        attributes: d[3],
                        transactions: ((packet >> 11) & 3) as u8 + 1,
                    });
                }
                DESC_HID => {
                    let interface = config.interfaces.last_mut().ok_or("HID descriptor without interface")?;
                    // Descriptor type 0x21 belongs to other classes too (DFU).
                    if interface.class.0 == CLASS_HID {
                        interface.report_length = interface.report_length.or_else(|| hid_report_length(d));
                    }
                }
                _ => {}
            }
            at += len;
        }
        if config.interfaces.last().is_some_and(|i| i.endpoints.len() != expected_endpoints) {
            return Err("USB interface endpoint count mismatch".into());
        }
        let numbers: BTreeSet<_> = config.interfaces.iter().map(|i| i.number).collect();
        if numbers.len() != usize::from(bytes[4]) {
            return Err("USB configuration interface count mismatch".into());
        }
        let mut active_endpoints = BTreeSet::new();
        for number in numbers {
            let active = config
                .interfaces
                .iter()
                .find(|i| i.number == number && i.alternate == 0)
                .ok_or("USB interface has no default alternate setting")?;
            for endpoint in &active.endpoints {
                if !active_endpoints.insert(endpoint.address) {
                    return Err("USB endpoint shared by active interfaces".into());
                }
            }
        }
        Ok(config)
    }
}

fn hid_report_length(descriptor: &[u8]) -> Option<u16> {
    let count = *descriptor.get(5)?;
    if descriptor.len() != 6 + 3 * usize::from(count) {
        return None;
    }
    let mut reports = descriptor[6..].as_chunks::<3>().0.iter().filter(|item| item[0] == DESC_REPORT);
    let [_, lo, hi] = *reports.next()?;
    let length = u16::from_le_bytes([lo, hi]);
    (length != 0 && reports.next().is_none()).then_some(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration(parts: &[&[u8]], count: u8) -> Vec<u8> {
        let mut bytes = vec![9, 2, 0, 0, count, 7, 0, 0x80, 50];
        for part in parts {
            bytes.extend_from_slice(part);
        }
        let length = (bytes.len() as u16).to_le_bytes();
        bytes[2..4].copy_from_slice(&length);
        bytes
    }

    #[test]
    fn interface_numbers_alternates_and_endpoints_are_not_descriptor_positions() {
        let c = configuration(
            &[
                &[9, 4, 7, 2, 1, 3, 0, 0, 0],
                &[7, 5, 0x85, 3, 64, 0, 10],
                &[9, 4, 3, 0, 2, 8, 6, 0x50, 0],
                &[7, 5, 0x82, 2, 64, 0, 0],
                &[7, 5, 4, 2, 64, 0, 0],
                &[9, 4, 7, 0, 1, 3, 1, 1, 0],
                &[9, 0x21, 0x11, 1, 0, 1, 0x22, 63, 0],
                &[7, 5, 0x81, 3, 8, 0, 10],
            ],
            2,
        );
        let parsed = Configuration::parse(&c).unwrap();
        assert_eq!(parsed.value, 7);
        assert_eq!(parsed.interfaces.len(), 3);
        assert_eq!(parsed.interfaces[0].alternate, 2);
        assert_eq!(parsed.interfaces[1].endpoints.iter().map(|e| e.address).collect::<Vec<_>>(), [0x82, 4]);
        assert_eq!(parsed.interfaces[2].report_length, Some(63));
        assert_eq!(parsed.interfaces[2].endpoints[0].address, 0x81);
    }

    #[test]
    fn configurations_larger_than_512_bytes_remain_complete() {
        let mut parts = vec![vec![9, 4, 12, 0, 1, 3, 0, 0, 0]];
        for _ in 0..8 {
            let mut vendor = vec![0; 100];
            vendor[0] = 100;
            vendor[1] = 0x40;
            parts.push(vendor);
        }
        parts.extend([vec![9, 0x21, 0x11, 1, 0, 1, 0x22, 0xff, 0xff], vec![7, 5, 0x83, 3, 64, 0, 1]]);
        let parts: Vec<_> = parts.iter().map(Vec::as_slice).collect();
        let bytes = configuration(&parts, 1);
        assert!(bytes.len() > 512);
        let parsed = Configuration::parse(&bytes).unwrap();
        assert_eq!(parsed.interfaces[0].number, 12);
        assert_eq!(parsed.interfaces[0].report_length, Some(u16::MAX));
        assert_eq!(parsed.interfaces[0].endpoints[0].address, 0x83);
    }

    #[test]
    fn malformed_lengths_counts_ranges_and_endpoint_relationships_are_rejected() {
        let valid = configuration(
            &[&[9, 4, 0, 0, 1, 3, 0, 0, 0], &[9, 0x21, 0x11, 1, 0, 1, 0x22, 20, 0], &[7, 5, 0x81, 3, 8, 0, 1]],
            1,
        );
        for end in 0..valid.len() {
            assert!(Configuration::parse(&valid[..end]).is_err());
        }
        for (at, value) in [(0, 8), (2, 9), (4, 2), (5, 0), (9, 0), (13, 2), (29, 0x80), (31, 0)] {
            let mut bytes = valid.clone();
            bytes[at] = value;
            assert!(Configuration::parse(&bytes).is_err(), "byte {at} = {value}");
        }
        let duplicate = configuration(
            &[
                &[9, 4, 0, 0, 1, 3, 0, 0, 0],
                &[7, 5, 0x81, 3, 8, 0, 1],
                &[9, 4, 2, 0, 1, 3, 0, 0, 0],
                &[7, 5, 0x81, 3, 8, 0, 1],
            ],
            2,
        );
        assert!(Configuration::parse(&duplicate).is_err());
        let no_default = configuration(&[&[9, 4, 0, 1, 0, 3, 0, 0, 0]], 1);
        assert!(Configuration::parse(&no_default).is_err());
    }

    #[test]
    fn a_bad_hid_class_descriptor_does_not_hide_the_other_interfaces() {
        let bytes = configuration(
            &[
                &[9, 4, 2, 0, 1, 3, 1, 1, 0],
                &[9, 0x21, 0x11, 1, 0, 2, 0x22, 63, 0],
                &[7, 5, 0x81, 3, 8, 0, 1],
                &[9, 4, 5, 0, 1, 3, 0, 0, 0],
                &[9, 0x21, 0x11, 1, 0, 1, 0x22, 30, 0],
                &[7, 5, 0x83, 3, 64, 0, 1],
            ],
            2,
        );
        let config = Configuration::parse(&bytes).unwrap();
        assert_eq!(config.interfaces[0].report_length, None);
        assert_eq!(config.interfaces[1].report_length, Some(30));
    }
}

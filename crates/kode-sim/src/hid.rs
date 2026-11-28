//! HID 1.11 report descriptors and reports, independent of USB and the UI.
//!
//! Layouts retain main-item groups and compact usage ranges; Report Count
//! never drives descriptor-time allocation. USB's u16 transfer length bounds
//! every report and descriptor. All offsets exclude the optional ID byte.

use std::collections::{BTreeMap, BTreeSet};

pub const PAGE_DESKTOP: u16 = 0x01;
pub const PAGE_KEYBOARD: u16 = 0x07;
pub const PAGE_BUTTON: u16 = 0x09;
pub const USAGE_MOUSE: u16 = 0x02;
pub const USAGE_KEYBOARD: u16 = 0x06;
pub const USAGE_KEYPAD: u16 = 0x07;
pub const USAGE_X: u16 = 0x30;
pub const USAGE_Y: u16 = 0x31;
pub const USAGE_WHEEL: u16 = 0x38;
pub const MODIFIER_FIRST: u16 = 0xe0;
pub const MODIFIER_LAST: u16 = 0xe7;
const MAX_TRANSFER: usize = u16::MAX as usize;
/// Bound the button usages represented in each report and page snapshot.
const MAX_BUTTON: u16 = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn invalid(s: impl Into<String>) -> Error {
    Error(s.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub page: u16,
    pub id: u16,
}

impl Usage {
    fn from_item(value: u32, bytes: usize, page: u16) -> Self {
        Self { page: if bytes == 4 { (value >> 16) as u16 } else { page }, id: value as u16 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Collection {
    usage: Option<Usage>,
    kind: u8,
}

impl Collection {
    fn keyboard(&self) -> bool {
        self.kind == 1
            && self.usage.is_some_and(|u| u.page == PAGE_DESKTOP && matches!(u.id, USAGE_KEYBOARD | USAGE_KEYPAD))
    }

    fn mouse(&self) -> bool {
        self.kind == 1 && self.usage == Some(Usage { page: PAGE_DESKTOP, id: USAGE_MOUSE })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReportKind {
    Input,
    Output,
    Feature,
}

#[derive(Debug, Clone, Default)]
struct Globals {
    page: u16,
    logical_min: i64,
    logical_max: i64,
    logical_max_item: Option<(u32, usize)>,
    physical_min: i64,
    physical_max: i64,
    physical_max_item: Option<(u32, usize)>,
    unit: u32,
    unit_exponent: i32,
    size: u32,
    count: u32,
    id: u8,
}

#[derive(Debug, Clone, Default)]
struct Locals {
    // Explicit usages and ranges preserve declaration order without expansion.
    usages: Vec<(Usage, Usage)>,
    ends: Vec<usize>,
    minimum: Option<Usage>,
    minimum_position: usize,
    delimiter_open: bool,
    delimiter_branches: usize,
}

impl Locals {
    fn alternate(&self) -> bool {
        self.delimiter_open && self.delimiter_branches > 1
    }

    fn delimiter(&mut self, value: u32) -> Result<(), Error> {
        match value {
            1 if !self.delimiter_open && self.minimum.is_none() => {
                self.delimiter_open = true;
                self.delimiter_branches += 1;
            }
            0 if self.delimiter_open && self.minimum.is_none() => self.delimiter_open = false,
            _ => return Err(invalid("invalid HID Delimiter")),
        }
        Ok(())
    }

    fn push(&mut self, first: Usage, last: Usage) {
        let end = self.ends.last().copied().unwrap_or(0) + usize::from(last.id - first.id) + 1;
        self.usages.push((first, last));
        self.ends.push(end);
    }

    fn nth(&self, index: usize, repeat: bool) -> Option<Usage> {
        let position = self.ends.partition_point(|end| *end <= index);
        if let Some((first, _)) = self.usages.get(position) {
            let start = position.checked_sub(1).map_or(0, |previous| self.ends[previous]);
            return Some(Usage { page: first.page, id: first.id + (index - start) as u16 });
        }
        repeat.then(|| self.usages.last().map(|(_, last)| *last)).flatten()
    }

    fn range(&mut self, first: Usage, last: Usage) {
        if self.minimum_position == self.usages.len() {
            self.push(first, last);
        } else {
            self.usages.insert(self.minimum_position, (first, last));
            self.ends.clear();
            let mut count = 0;
            for (first, last) in &self.usages {
                count += usize::from(last.id - first.id) + 1;
                self.ends.push(count);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Field {
    offset: usize,
    globals: Globals,
    locals: Locals,
    flags: u32,
    application: Option<usize>,
}

impl Field {
    fn decodable(&self) -> bool {
        !self.constant() && self.globals.count != 0 && self.globals.size <= 32 && self.flags & (1 << 8) == 0
    }

    fn constant(&self) -> bool {
        self.flags & 1 != 0
    }

    fn variable(&self) -> bool {
        self.flags & 2 != 0
    }

    fn relative(&self) -> bool {
        self.flags & 4 != 0
    }

    fn usages(&self) -> impl Iterator<Item = (Usage, Usage)> + '_ {
        let mut remaining = if self.globals.count == 0 {
            0
        } else if self.variable() {
            u64::from(self.globals.count)
        } else {
            (self.globals.logical_max - self.globals.logical_min + 1) as u64
        };
        self.locals.usages.iter().filter_map(move |(first, last)| {
            let used = remaining.min(u64::from(last.id - first.id) + 1);
            remaining -= used;
            (used > 0).then(|| (*first, Usage { page: first.page, id: (u64::from(first.id) + used - 1) as u16 }))
        })
    }
}

#[derive(Debug, Clone, Default)]
struct Layout {
    bits: usize,
    fields: Vec<Field>,
}

#[derive(Debug, Clone)]
pub struct Descriptor {
    reports: BTreeMap<(ReportKind, u8), Layout>,
    collections: Vec<Collection>,
    pub diagnostics: Vec<String>,
    pub has_ids: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keyboard {
    pub keys: BTreeSet<u16>,
    pub modifiers: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mouse {
    pub buttons: BTreeSet<u16>,
    pub relative: BTreeMap<u16, i64>,
    pub absolute: BTreeMap<u16, i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputCollection {
    pub collection: usize,
    pub keyboard: Option<Keyboard>,
    pub mouse: Option<Mouse>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub report_id: Option<u8>,
    pub collections: Vec<InputCollection>,
}

#[derive(Debug, Clone, Copy)]
pub enum Boot {
    Keyboard,
    Mouse,
}

impl Boot {
    pub fn input(self, report: &[u8]) -> Result<Input, Error> {
        let mut collection = InputCollection::default();
        match (self, report) {
            // HID 1.11 Appendix B.1.
            (Self::Keyboard, [modifiers, _, keys @ ..]) if keys.len() == 6 => {
                if keys.iter().all(|key| !matches!(key, 1..=3)) {
                    collection.keyboard = Some(Keyboard {
                        keys: keys.iter().copied().filter(|key| *key != 0).map(u16::from).collect(),
                        modifiers: *modifiers,
                    });
                }
            }
            // HID 1.11 Appendix B.2.
            (Self::Mouse, [buttons, x, y]) => {
                collection.mouse = Some(Mouse {
                    buttons: (1..=3).filter(|id| buttons & (1 << (id - 1)) != 0).collect(),
                    relative: BTreeMap::from([(USAGE_X, i64::from(*x as i8)), (USAGE_Y, i64::from(*y as i8))]),
                    absolute: BTreeMap::new(),
                });
            }
            _ => return Err(invalid("invalid HID boot report length")),
        }
        let collections = (collection.keyboard.is_some() || collection.mouse.is_some()).then_some(collection);
        Ok(Input { report_id: None, collections: collections.into_iter().collect() })
    }
}

fn unsigned(data: &[u8]) -> u32 {
    data.iter().enumerate().fold(0, |v, (i, b)| v | (u32::from(*b) << (i * 8)))
}

fn signed(value: u32, bytes: usize) -> i64 {
    let shift = 64 - bytes * 8;
    (i64::from(value) << shift) >> shift
}

impl Descriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.is_empty() || bytes.len() > MAX_TRANSFER {
            return Err(invalid("HID descriptor length is outside a USB control transfer"));
        }
        let mut out = Self { reports: BTreeMap::new(), collections: vec![], diagnostics: vec![], has_ids: false };
        let mut globals = Globals::default();
        let mut locals = Locals::default();
        let mut stack = Vec::new();
        let mut collections: Vec<usize> = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let prefix = bytes[at];
            at += 1;
            if prefix == 0xfe {
                let header = bytes.get(at..at + 2).ok_or_else(|| invalid("truncated HID long-item header"))?;
                let size = usize::from(header[0]);
                at += 2;
                bytes.get(at..at + size).ok_or_else(|| invalid("truncated HID long item"))?;
                out.diagnostics.push(format!("unsupported HID long item {:#04x} ({size} bytes)", header[1]));
                at += size;
                continue;
            }
            let size = match prefix & 3 {
                3 => 4,
                n => usize::from(n),
            };
            let data = bytes.get(at..at + size).ok_or_else(|| invalid("truncated HID short item"))?;
            at += size;
            let value = unsigned(data);
            let item_type = (prefix >> 2) & 3;
            let tag = prefix >> 4;
            if size == 0 && !matches!((item_type, tag), (0, 12) | (1, 10 | 11)) {
                return Err(invalid("HID item requires data"));
            }
            match (item_type, tag) {
                (1, 0) => {
                    globals.page = u16::try_from(value).map_err(|_| invalid("Usage Page exceeds 16 bits"))?;
                }
                (1, 1) => {
                    globals.logical_min = signed(value, size);
                    if let Some((maximum, size)) = globals.logical_max_item {
                        globals.logical_max =
                            if globals.logical_min < 0 { signed(maximum, size) } else { i64::from(maximum) };
                    }
                }
                (1, 2) => {
                    globals.logical_max_item = Some((value, size));
                    globals.logical_max = if globals.logical_min < 0 { signed(value, size) } else { i64::from(value) }
                }
                (1, 3) => {
                    globals.physical_min = signed(value, size);
                    if let Some((maximum, size)) = globals.physical_max_item {
                        globals.physical_max =
                            if globals.physical_min < 0 { signed(maximum, size) } else { i64::from(maximum) };
                    }
                }
                (1, 4) => {
                    globals.physical_max_item = Some((value, size));
                    globals.physical_max = if globals.physical_min < 0 { signed(value, size) } else { i64::from(value) }
                }
                (1, 5) => {
                    // HID encodes the exponent in a signed four-bit nibble.
                    let exponent =
                        if value <= 15 { i64::from(((value as i32) << 28) >> 28) } else { signed(value, size) };
                    if !(-8..=7).contains(&exponent) {
                        return Err(invalid("invalid HID Unit Exponent"));
                    }
                    globals.unit_exponent = exponent as i32;
                }
                (1, 6) => globals.unit = value,
                (1, 7) => globals.size = value,
                (1, 8) => {
                    globals.id = u8::try_from(value)
                        .ok()
                        .filter(|id| *id != 0)
                        .ok_or_else(|| invalid("invalid HID Report ID"))?;
                    out.has_ids = true;
                }
                (1, 9) => globals.count = value,
                (1, 10) if size == 0 => stack.push(globals.clone()),
                (1, 11) if size == 0 => globals = stack.pop().ok_or_else(|| invalid("HID Pop without Push"))?,
                (2, 0..=2) if locals.alternate() => {}
                (2, 0) => {
                    let u = Usage::from_item(value, size, globals.page);
                    locals.push(u, u);
                }
                (2, 1) => {
                    locals.minimum_position = locals.usages.len();
                    if locals.minimum.replace(Usage::from_item(value, size, globals.page)).is_some() {
                        return Err(invalid("Usage Minimum without matching Maximum"));
                    }
                }
                (2, 2) => {
                    let first = locals.minimum.take().ok_or_else(|| invalid("Usage Maximum without Minimum"))?;
                    let last = Usage::from_item(value, size, globals.page);
                    if first.page != last.page || first.id > last.id {
                        return Err(invalid("invalid HID usage range"));
                    }
                    locals.range(first, last);
                }
                // Designator and string metadata do not change report layout.
                (2, 3..=5 | 7..=9) => {}
                // A field can spell out alternative names for the same usages.
                // Keep the first set and consume the remaining sets.
                (2, 10) => locals.delimiter(value)?,
                (0, _) => {
                    if locals.minimum.is_some() || locals.delimiter_open {
                        return Err(invalid("unfinished HID usage range or Delimiter"));
                    }
                    match tag {
                        10 => {
                            let kind = u8::try_from(value).map_err(|_| invalid("invalid HID collection type"))?;
                            let collection = Collection { kind, usage: locals.nth(0, false) };
                            if kind > 6 {
                                out.diagnostics.push(format!("unsupported HID collection type {kind}"));
                            }
                            if kind == 1 && !collection.keyboard() && !collection.mouse() {
                                out.diagnostics.push(format!("unsupported HID application {:?}", collection.usage));
                            }
                            collections.push(out.collections.len());
                            out.collections.push(collection);
                        }
                        12 if size == 0 => {
                            collections.pop().ok_or_else(|| invalid("HID End Collection without Collection"))?;
                        }
                        8 | 9 | 11 => {
                            if collections.is_empty() {
                                return Err(invalid("HID data outside a collection"));
                            }
                            if globals.logical_min > globals.logical_max {
                                return Err(invalid("reversed HID logical range"));
                            }
                            if globals.count != 0 && globals.size == 0 {
                                return Err(invalid("zero HID Report Size"));
                            }
                            let kind = match tag {
                                8 => ReportKind::Input,
                                9 => ReportKind::Output,
                                _ => ReportKind::Feature,
                            };
                            if value & !0x01ff != 0 || (kind == ReportKind::Input && value & 0x80 != 0) {
                                return Err(invalid("reserved HID main-item flags"));
                            }
                            let bits = u64::from(globals.size) * u64::from(globals.count);
                            let limit = (MAX_TRANSFER - usize::from(globals.id != 0)) * 8;
                            let layout = out.reports.entry((kind, globals.id)).or_default();
                            if bits > limit as u64 || layout.bits as u64 + bits > limit as u64 {
                                return Err(invalid("HID report exceeds the USB transfer length"));
                            }
                            let application = collections.iter().rev().copied().find(|i| out.collections[*i].kind == 1);
                            let field = Field {
                                offset: layout.bits,
                                globals: globals.clone(),
                                locals: locals.clone(),
                                flags: value,
                                application,
                            };
                            if !field.constant() && kind == ReportKind::Input {
                                if globals.size > 32 || value & (1 << 8) != 0 {
                                    out.diagnostics
                                        .push(format!("unsupported HID input encoding in report {}", globals.id));
                                }
                                for (first, last) in &locals.usages {
                                    let supported = application.is_some_and(|i| {
                                        let c = &out.collections[i];
                                        (c.keyboard() && first.page == PAGE_KEYBOARD)
                                            || (c.mouse()
                                                && (first.page == PAGE_BUTTON
                                                    || (first.page == PAGE_DESKTOP
                                                        && ((first.id >= USAGE_X && last.id <= USAGE_Y)
                                                            || (first.id == USAGE_WHEEL && last.id == USAGE_WHEEL)))))
                                    });
                                    if !supported {
                                        out.diagnostics.push(format!(
                                            "unsupported HID input usage {first:?}..{last:?} in report {}",
                                            globals.id
                                        ));
                                    }
                                }
                            }
                            layout.bits += bits as usize;
                            layout.fields.push(field);
                        }
                        _ => return Err(invalid(format!("unsupported HID main item {tag}"))),
                    }
                    locals = Locals::default();
                }
                _ => return Err(invalid(format!("unsupported HID item type {item_type}, tag {tag}"))),
            }
        }
        if !collections.is_empty()
            || !stack.is_empty()
            || locals.minimum.is_some()
            || locals.delimiter_open
            || !locals.usages.is_empty()
        {
            return Err(invalid("unfinished HID collection, Push or local item"));
        }
        if out.has_ids && out.reports.keys().any(|(_, id)| *id == 0) {
            return Err(invalid("HID mixes numbered and unnumbered reports"));
        }
        Ok(out)
    }

    pub fn report_lengths(&self, kind: ReportKind) -> Vec<(Option<u8>, usize)> {
        self.reports
            .iter()
            .filter(|((k, _), _)| *k == kind)
            .map(|((_, id), layout)| (self.has_ids.then_some(*id), layout.bits.div_ceil(8) + usize::from(self.has_ids)))
            .collect()
    }

    pub fn keyboard(&self) -> bool {
        self.reports.iter().any(|((kind, _), layout)| {
            *kind == ReportKind::Input
                && layout.fields.iter().any(|f| {
                    f.decodable()
                        && !f.relative()
                        && f.application.is_some_and(|a| self.collections[a].keyboard())
                        && f.usages().any(|(u, _)| u.page == PAGE_KEYBOARD)
                })
        })
    }

    pub fn mouse(&self) -> bool {
        self.reports.iter().any(|((kind, _), layout)| {
            *kind == ReportKind::Input
                && layout.fields.iter().any(|f| {
                    f.decodable()
                        && f.application.is_some_and(|a| self.collections[a].mouse())
                        && f.usages().any(|(first, last)| {
                            (first.page == PAGE_BUTTON && !f.relative() && first.id <= MAX_BUTTON && last.id != 0)
                                || (first.page == PAGE_DESKTOP
                                    && f.variable()
                                    && ((first.id <= USAGE_Y && last.id >= USAGE_X)
                                        || (first.id <= USAGE_WHEEL && last.id >= USAGE_WHEEL)))
                        })
                })
        })
    }

    pub fn buttons(&self) -> Vec<u16> {
        let mut buttons = BTreeSet::new();
        for ((kind, _), layout) in &self.reports {
            if *kind != ReportKind::Input {
                continue;
            }
            for field in &layout.fields {
                if !field.decodable()
                    || field.relative()
                    || !field.application.is_some_and(|app| self.collections[app].mouse())
                {
                    continue;
                }
                for (first, last) in field.usages() {
                    if first.page == PAGE_BUTTON {
                        buttons.extend(first.id.max(1)..=last.id.min(MAX_BUTTON));
                    }
                }
            }
        }
        buttons.into_iter().collect()
    }

    /// Decode the usable fields of a complete report.
    pub fn input(&self, report: &[u8]) -> Result<Input, Error> {
        let (id, payload) = if self.has_ids {
            let (id, payload) = report.split_first().ok_or_else(|| invalid("missing HID Report ID"))?;
            (*id, payload)
        } else {
            (0, report)
        };
        let layout = self
            .reports
            .get(&(ReportKind::Input, id))
            .ok_or_else(|| invalid(format!("unknown HID input Report ID {id}")))?;
        if payload.len() != layout.bits.div_ceil(8) {
            return Err(invalid(format!(
                "HID report {id}: expected {} bytes, received {}",
                layout.bits.div_ceil(8),
                payload.len()
            )));
        }
        let mut decoded: BTreeMap<usize, InputCollection> = BTreeMap::new();
        let mut rollover = BTreeSet::new();
        for field in &layout.fields {
            if !field.decodable() {
                continue;
            }
            let Some(app) = field.application else { continue };
            let collection = &self.collections[app];
            let relevant = field.usages().any(|(first, last)| {
                (collection.keyboard() && first.page == PAGE_KEYBOARD)
                    || (collection.mouse()
                        && (first.page == PAGE_BUTTON
                            || (first.page == PAGE_DESKTOP
                                && ((first.id <= USAGE_Y && last.id >= USAGE_X)
                                    || (first.id <= USAGE_WHEEL && last.id >= USAGE_WHEEL)))))
            });
            if !relevant {
                continue;
            }
            let entry = decoded.entry(app).or_insert_with(|| InputCollection {
                collection: app,
                keyboard: collection.keyboard().then(Keyboard::default),
                mouse: collection.mouse().then(Mouse::default),
            });
            for index in 0..field.globals.count as usize {
                let offset = field.offset + index * field.globals.size as usize;
                let raw = bits(payload, offset, field.globals.size as usize);
                let value = if field.globals.logical_min < 0 {
                    let shift = 64 - field.globals.size;
                    (i64::from(raw) << shift) >> shift
                } else {
                    i64::from(raw)
                };
                if value < field.globals.logical_min || value > field.globals.logical_max {
                    continue;
                }
                let usage = if field.variable() {
                    field.locals.nth(index, true)
                } else {
                    field.locals.nth((value - field.globals.logical_min) as usize, false)
                };
                let Some(usage) = usage else { continue };
                if let Some(keyboard) = &mut entry.keyboard {
                    if usage.page != PAGE_KEYBOARD || field.relative() || (field.variable() && value == 0) {
                        continue;
                    }
                    match usage.id {
                        0 => {}
                        1..=3 => {
                            rollover.insert(app);
                        }
                        MODIFIER_FIRST..=MODIFIER_LAST => keyboard.modifiers |= 1 << (usage.id - MODIFIER_FIRST),
                        key => {
                            keyboard.keys.insert(key);
                        }
                    }
                }
                if let Some(mouse) = &mut entry.mouse {
                    if usage.page == PAGE_BUTTON {
                        if !field.relative()
                            && (1..=MAX_BUTTON).contains(&usage.id)
                            && (!field.variable() || value != 0)
                        {
                            mouse.buttons.insert(usage.id);
                        }
                    } else if field.variable()
                        && usage.page == PAGE_DESKTOP
                        && matches!(usage.id, USAGE_X | USAGE_Y | USAGE_WHEEL)
                    {
                        if field.relative() {
                            let current = mouse.relative.entry(usage.id).or_default();
                            *current += value;
                        } else {
                            mouse.absolute.entry(usage.id).or_insert(value);
                        }
                    }
                }
            }
        }
        // Rollover leaves this keyboard's last good state in place. Other
        // collections in the same report can still update.
        decoded.retain(|app, _| !rollover.contains(app));
        Ok(Input { report_id: self.has_ids.then_some(id), collections: decoded.into_values().collect() })
    }
}

/// Caller has checked the complete report length and the group's bounds.
fn bits(payload: &[u8], offset: usize, width: usize) -> u32 {
    let mut value = 0;
    for bit in 0..width {
        value |= u32::from((payload[(offset + bit) / 8] >> ((offset + bit) % 8)) & 1) << bit;
    }
    value
}

#[cfg(test)]
pub(crate) mod test_support {
    pub fn hex(text: &str) -> Vec<u8> {
        text.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    pub fn keyboard() -> Vec<u8> {
        hex(include_str!("../../../tests/fixtures/hid/keyboard.hex"))
    }
    pub fn mouse() -> Vec<u8> {
        hex(include_str!("../../../tests/fixtures/hid/mouse.hex"))
    }

    pub fn numbered(mut bytes: Vec<u8>, id: u8) -> Vec<u8> {
        // Global Report ID applies to the complete descriptor.
        bytes.splice(..0, [0x85, id]);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::*;

    fn keyboard_report(d: &Descriptor, bytes: &[u8]) -> Keyboard {
        d.input(bytes).unwrap().collections.remove(0).keyboard.unwrap()
    }
    fn mouse_report(d: &Descriptor, bytes: &[u8]) -> Mouse {
        d.input(bytes).unwrap().collections.remove(0).mouse.unwrap()
    }

    #[test]
    fn standard_keyboard_arrays_modifiers_and_independent_output_offsets() {
        let d = Descriptor::parse(&keyboard()).unwrap();
        assert!(d.keyboard() && !d.mouse() && !d.has_ids);
        assert!(d.diagnostics.is_empty());
        assert_eq!(d.report_lengths(ReportKind::Input), [(None, 8)]);
        assert_eq!(d.report_lengths(ReportKind::Output), [(None, 1)]);
        let k = keyboard_report(&d, &[0x82, 0, 4, 5, 6, 0, 0, 0]);
        assert_eq!(k.modifiers, 0x82);
        assert_eq!(k.keys, BTreeSet::from([4, 5, 6]));
        assert_eq!(keyboard_report(&d, &[0; 8]), Keyboard::default());
    }

    #[test]
    fn descriptors_emitted_by_the_resolved_real_tinyusb_macros_decode() {
        let keyboard = hex(include_str!("../../../tests/fixtures/hid/tinyusb_keyboard.hex"));
        let mouse = hex(include_str!("../../../tests/fixtures/hid/tinyusb_mouse.hex"));
        let k = Descriptor::parse(&keyboard).unwrap();
        assert_eq!(
            keyboard_report(&k, &[0x42, 0, 4, 5, 0, 0, 0, 0]),
            Keyboard { keys: BTreeSet::from([4, 5]), modifiers: 0x42 }
        );
        let m = Descriptor::parse(&mouse).unwrap();
        assert!(
            m.diagnostics.iter().any(|d| d.contains("unsupported HID input usage")),
            "horizontal pan is explicitly diagnosed"
        );
        assert_eq!(
            mouse_report(&m, &[0x15, 12, 249, 255, 17]).relative,
            BTreeMap::from([(USAGE_X, 12), (USAGE_Y, -7), (USAGE_WHEEL, -1)])
        );
        assert_eq!(mouse_report(&m, &[0, 12, 249, 255, 128]).relative[&USAGE_X], 12);
        let mut shared = numbered(keyboard, 43);
        shared.extend(numbered(mouse, 207));
        let d = Descriptor::parse(&shared).unwrap();
        assert_eq!(d.report_lengths(ReportKind::Input), [(Some(43), 9), (Some(207), 6)]);
        assert_eq!(keyboard_report(&d, &[43, 0, 0, 4, 0, 0, 0, 0, 0]).keys, BTreeSet::from([4]));
        assert_eq!(mouse_report(&d, &[207, 2, 255, 1, 0, 0]).buttons, BTreeSet::from([2]));
    }

    #[test]
    fn report_ids_are_declared_and_never_inferred_from_report_length() {
        let d = Descriptor::parse(&numbered(keyboard(), 73)).unwrap();
        assert_eq!(d.report_lengths(ReportKind::Input), [(Some(73), 9)]);
        assert_eq!(keyboard_report(&d, &[73, 2, 0, 4, 0, 0, 0, 0, 0]).keys, BTreeSet::from([4]));
        assert!(d.input(&[1, 2, 0, 4, 0, 0, 0, 0, 0]).is_err());
        assert!(d.input(&[2, 0, 4, 0, 0, 0, 0, 0]).is_err());
        assert!(Descriptor::parse(&keyboard()).unwrap().input(&[1, 2, 0, 4, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn mouse_buttons_signed_axes_and_wheel_with_and_without_ids() {
        for id in [None, Some(149)] {
            let bytes = id.map_or_else(mouse, |id| numbered(mouse(), id));
            let d = Descriptor::parse(&bytes).unwrap();
            assert!(!d.keyboard() && d.mouse());
            assert!(d.diagnostics.is_empty());
            let mut report: Vec<_> = id.into_iter().collect();
            report.extend([0b10101, 12, (-42_i8) as u8, (-1_i8) as u8]);
            let m = mouse_report(&d, &report);
            assert_eq!(m.buttons, BTreeSet::from([1, 3, 5]));
            assert_eq!(m.relative, BTreeMap::from([(USAGE_X, 12), (USAGE_Y, -42), (USAGE_WHEEL, -1)]));
            assert!(m.absolute.is_empty());
        }
    }

    #[test]
    fn declared_mouse_buttons_are_available_before_any_input() {
        assert_eq!(Descriptor::parse(&mouse()).unwrap().buttons(), [1, 2, 3, 4, 5]);
        assert!(Descriptor::parse(&keyboard()).unwrap().buttons().is_empty());
        let sparse = hex("05 01 09 02 a1 01 05 09 09 02 09 07 09 09
            15 00 25 01 75 01 95 02 81 02 75 06 95 01 81 01 c0");
        assert_eq!(Descriptor::parse(&sparse).unwrap().buttons(), [2, 7]);
        let array = hex("05 01 09 02 a1 01 05 09 19 00 29 08
            15 00 25 04 75 08 95 01 81 00 c0");
        assert_eq!(Descriptor::parse(&array).unwrap().buttons(), [1, 2, 3, 4]);
        let mut multiple = test_support::numbered(mouse(), 43);
        multiple.extend(test_support::numbered(sparse, 207));
        assert_eq!(Descriptor::parse(&multiple).unwrap().buttons(), [1, 2, 3, 4, 5, 7]);
    }

    #[test]
    fn sixteen_bit_axes_unaligned_and_reordered_buttons() {
        // A 3-bit constant prefix, Y then X as signed 16-bit fields,
        // four buttons, and one constant bit: 40 input bits altogether.
        let d = Descriptor::parse(&hex("05 01 09 02 a1 01 75 03 95 01 81 01
            09 31 09 30 16 00 80 26 ff 7f 75 10 95 02 81 06
            05 09 19 01 29 04 15 00 25 01 75 01 95 04 81 02
            75 01 95 01 81 01 c0"))
        .unwrap();
        let packed = ((-32768_i16 as u16 as u64) << 3) | (32767_u64 << 19) | (0b1010_u64 << 35);
        let bytes = packed.to_le_bytes();
        let m = mouse_report(&d, &bytes[..5]);
        assert_eq!(m.relative[&USAGE_Y], -32768);
        assert_eq!(m.relative[&USAGE_X], 32767);
        assert_eq!(m.buttons, BTreeSet::from([2, 4]));
    }

    #[test]
    fn bitmap_keyboard_extended_usages_and_padding() {
        let d = Descriptor::parse(&hex("05 01 09 06 a1 01 05 07 19 04 29 0b
            15 00 25 01 75 01 95 08 81 02 c0"))
        .unwrap();
        assert_eq!(keyboard_report(&d, &[0b10000011]).keys, BTreeSet::from([4, 5, 11]));
        let wide = Descriptor::parse(&hex("05 01 09 06 a1 01 0b 34 12 07 00
            15 00 25 01 75 01 95 01 81 02 75 07 95 01 81 01 c0"))
        .unwrap();
        assert_eq!(keyboard_report(&wide, &[1]).keys, BTreeSet::from([0x1234]));
    }

    #[test]
    fn array_selectors_use_logical_minimum_and_explicit_usage_order() {
        let d = Descriptor::parse(&hex("05 01 09 06 a1 01 05 07 09 08 09 04 09 05
            15 0a 25 0c 75 08 95 02 81 00 c0"))
        .unwrap();
        assert_eq!(keyboard_report(&d, &[10, 12]).keys, BTreeSet::from([8, 5]));
        assert_eq!(keyboard_report(&d, &[0, 12]).keys, BTreeSet::from([5]));
        let d = Descriptor::parse(&hex("05 01 09 06 a1 01 05 07 19 04 09 08 29 06
            15 00 25 03 75 08 95 04 81 00 c0"))
        .unwrap();
        assert_eq!(keyboard_report(&d, &[0, 1, 2, 3]).keys, BTreeSet::from([4, 5, 6, 8]));
    }

    #[test]
    fn collections_and_each_kind_and_id_have_separate_offsets() {
        let mut bytes = numbered(keyboard(), 71);
        bytes.extend(numbered(mouse(), 203));
        let d = Descriptor::parse(&bytes).unwrap();
        assert!(d.keyboard() && d.mouse());
        assert_eq!(d.report_lengths(ReportKind::Input), [(Some(71), 9), (Some(203), 5)]);
        let mouse = d.input(&[203, 3, 2, 255, 1]).unwrap();
        assert!(mouse.collections[0].keyboard.is_none());
        assert_eq!(mouse.collections[0].collection, 1);
        let keyboard = d.input(&[71, 0, 0, 4, 0, 0, 0, 0, 0]).unwrap();
        assert!(keyboard.collections[0].mouse.is_none());
        assert_eq!(d.report_lengths(ReportKind::Output), [(Some(71), 2)]);
    }

    #[test]
    fn two_collections_can_share_an_unnumbered_report() {
        let mut bytes = keyboard();
        bytes.extend(mouse());
        let d = Descriptor::parse(&bytes).unwrap();
        let i = d.input(&[0, 0, 4, 0, 0, 0, 0, 0, 2, 1, 255, 3]).unwrap();
        assert_eq!(i.collections.len(), 2);
        assert_eq!(i.collections[0].keyboard.as_ref().unwrap().keys, BTreeSet::from([4]));
        assert_eq!(i.collections[1].mouse.as_ref().unwrap().buttons, BTreeSet::from([2]));
    }

    #[test]
    fn push_pop_restore_all_globals_and_feature_offsets_are_independent() {
        let d = Descriptor::parse(&hex("05 01 09 02 a1 01 15 81 25 7f 75 08 95 01
            a4 85 2b 05 09 09 01 15 00 25 01 75 01 95 01 81 02
            75 07 95 01 81 01 b4 09 30 81 06 09 38 b1 02 c0"));
        assert!(d.is_err(), "Pop restores report zero, which cannot mix with numbered reports");
        let d = Descriptor::parse(&hex("85 2b 05 01 09 02 a1 01 15 81 25 7f 75 08 95 01
            a4 05 09 09 01 15 00 25 01 75 01 95 01 81 02
            75 07 95 01 81 01 b4 09 30 81 06 09 38 b1 02 c0"))
        .unwrap();
        assert_eq!(d.report_lengths(ReportKind::Feature), [(Some(43), 2)]);
        let m = mouse_report(&d, &[43, 1, 254]);
        assert_eq!(m.buttons, BTreeSet::from([1]));
        assert_eq!(m.relative[&USAGE_X], -2);
    }

    #[test]
    fn unsigned_absolute_axes_are_preserved_and_never_become_deltas() {
        let d = Descriptor::parse(&hex("05 01 09 02 a1 01 09 30 15 00 26 ff ff 75 10 95 01
            35 00 46 ff 7f 55 0e 65 11 81 02 c0"))
        .unwrap();
        let m = mouse_report(&d, &[255, 255]);
        assert_eq!(m.absolute[&USAGE_X], 65535);
        assert!(m.relative.is_empty());
    }

    #[test]
    fn signed_maximum_can_precede_minimum_and_signed_exponents_are_accepted() {
        let d = Descriptor::parse(&hex("05 01 09 02 a1 01 09 30 25 ff 15 81 55 fe
            75 08 95 01 81 06 c0"))
        .unwrap();
        assert_eq!(mouse_report(&d, &[255]).relative[&USAGE_X], -1);
        assert!(mouse_report(&d, &[1]).relative.is_empty());
    }

    #[test]
    fn null_array_selectors_are_absent_and_variable_usages_repeat_the_last_usage() {
        let d = Descriptor::parse(&hex("05 01 09 06 a1 01 05 07 09 04 09 05
            15 01 25 02 75 08 95 03 81 40 c0"))
        .unwrap();
        assert_eq!(keyboard_report(&d, &[1, 2, 0]).keys, BTreeSet::from([4, 5]));
        let d = Descriptor::parse(&hex("05 01 09 02 a1 01 09 30 15 81 25 7f
            75 08 95 02 81 06 c0"))
        .unwrap();
        assert_eq!(mouse_report(&d, &[1, 2]).relative[&USAGE_X], 3);
    }

    #[test]
    fn unsupported_encoding_does_not_claim_an_undecoded_report_succeeded() {
        let d = Descriptor::parse(&hex("05 01 09 02 a1 01 09 30 15 00 25 01
            75 21 95 01 81 02 c0"))
        .unwrap();
        assert!(!d.diagnostics.is_empty());
        assert!(!d.mouse());
        assert!(d.input(&[0; 5]).unwrap().collections.is_empty());
        let d = Descriptor::parse(&hex("05 01 09 02 a1 01 06 00 ff 09 01 15 00 25 01
            75 08 95 01 81 02 c0"))
        .unwrap();
        assert!(!d.mouse());
        assert!(d.input(&[1]).unwrap().collections.is_empty());
    }

    #[test]
    fn unknown_fields_are_diagnosed_without_becoming_keys_or_mouse_buttons() {
        let mut bytes = hex("05 01 09 06 a1 01 06 00 ff 09 01 15 00 25 01 75 08 95 01 81 02 c0");
        bytes.extend(mouse());
        let d = Descriptor::parse(&bytes).unwrap();
        assert!(!d.diagnostics.is_empty());
        let i = d.input(&[1, 0, 1, 2, 3]).unwrap();
        assert_eq!(i.collections.len(), 1);
        assert!(i.collections[0].keyboard.is_none());
        assert_eq!(i.collections[0].mouse.as_ref().unwrap().relative[&USAGE_X], 1);
        let unknown = Descriptor::parse(&hex("05 01 09 05 a1 01 09 30 15 00 25 01 75 08 95 01 81 02 c0")).unwrap();
        assert!(unknown.input(&[1]).unwrap().collections.is_empty());
    }

    #[test]
    fn supported_axes_inside_a_larger_usage_range_advertise_mouse_input() {
        let d = Descriptor::parse(&hex("05 01 09 02 a1 01 19 32 29 38 15 81 25 7f
            75 08 95 07 81 06 c0"))
        .unwrap();
        assert!(d.mouse());
        assert!(!d.keyboard());
        assert!(!d.diagnostics.is_empty());
        assert_eq!(mouse_report(&d, &[0, 0, 0, 0, 0, 0, 1]).relative, BTreeMap::from([(USAGE_WHEEL, 1)]));
    }

    #[test]
    fn usages_beyond_the_field_count_or_array_range_do_not_claim_input_support() {
        for encoding in ["75 08 95 01 81 06", "75 08 95 01 15 00 25 00 81 00"] {
            let d =
                Descriptor::parse(&hex(&format!("05 01 09 02 a1 01 19 32 29 38 15 81 25 7f {encoding} c0"))).unwrap();
            assert!(!d.mouse());
            assert!(d.input(&[0]).unwrap().collections.is_empty());
        }
        let d = Descriptor::parse(&hex("05 01 09 06 a1 01 0b 01 00 00 ff 0b 04 00 07 00
            15 00 25 01 75 01 95 01 81 02 75 07 95 01 81 01 c0"))
        .unwrap();
        assert!(!d.keyboard());
        assert!(d.input(&[0]).unwrap().collections.is_empty());
    }

    #[test]
    fn unsigned_32_bit_axes_keep_their_full_range() {
        let d = Descriptor::parse(&hex("05 01 09 02 a1 01 09 30 15 00 27 ff ff ff ff
            75 20 95 01 81 02 c0"))
        .unwrap();
        let m = mouse_report(&d, &u32::MAX.to_le_bytes());
        assert_eq!(m.absolute[&USAGE_X], i64::from(u32::MAX));
        assert!(m.relative.is_empty());
    }

    #[test]
    fn long_items_are_length_checked_and_explicitly_diagnosed() {
        let mut bytes = hex("fe 03 87 aa bb cc");
        bytes.extend(mouse());
        let d = Descriptor::parse(&bytes).unwrap();
        assert!(d.diagnostics[0].contains("long item"));
        assert_eq!(mouse_report(&d, &[0, 1, 2, 3]).relative[&USAGE_X], 1);
        for bytes in [hex("fe"), hex("fe 02"), hex("fe 03 87 aa")] {
            assert!(Descriptor::parse(&bytes).is_err());
        }
    }

    #[test]
    fn malformed_descriptors_and_numeric_extremes_fail_explicitly() {
        for text in [
            "",
            "05",
            "c0",
            "b4",
            "a4",
            "85 00",
            "86 00 01",
            "05 01 09 06 a1 01",
            "05 01 09 06 a1 01 05 07 29 04 c0",
            "05 01 09 06 a1 01 19 05 29 04 c0",
            "05 01 09 06 a1 01 09 04 15 01 25 00 75 01 95 01 81 02 c0",
            "05 01 09 06 a1 01 09 04 77 ff ff ff ff 97 ff ff ff ff 81 02 c0",
            "05 01 09 06 a1 01 09 04 75 00 95 01 81 02 c0",
        ] {
            assert!(Descriptor::parse(&hex(text)).is_err(), "{text}");
        }
        let d = Descriptor::parse(&hex("05 01 09 02 a1 01 09 30 17 00 00 00 80 27 ff ff ff 7f
            75 20 95 01 81 06 c0"))
        .unwrap();
        assert_eq!(mouse_report(&d, &i32::MIN.to_le_bytes()).relative[&USAGE_X], i64::from(i32::MIN));
        assert_eq!(mouse_report(&d, &i32::MAX.to_le_bytes()).relative[&USAGE_X], i64::from(i32::MAX));
    }

    #[test]
    fn invalid_reports_leave_a_prior_decoded_value_unchanged() {
        let d = Descriptor::parse(&keyboard()).unwrap();
        let valid = keyboard_report(&d, &[0, 0, 4, 0, 0, 0, 0, 0]);
        for report in [vec![], vec![0; 7], vec![0; 9]] {
            assert!(d.input(&report).is_err());
            assert_eq!(valid.keys, BTreeSet::from([4]));
        }
        assert!(d.input(&[0, 0, 1, 1, 1, 1, 1, 1]).unwrap().collections.is_empty());
        assert_eq!(keyboard_report(&d, &[0, 0, 250, 4, 0, 0, 0, 0]).keys, BTreeSet::from([4]));
    }

    #[test]
    fn delimiter_keeps_the_primary_usages_and_input_with_no_usage_still_consumes_bits() {
        let d = Descriptor::parse(&hex("05 01 09 06 a1 01 05 07 15 00 25 01 75 08 95 01
             a9 01 09 04 a9 00 a9 01 09 05 a9 00 81 02
             81 02 09 06 81 02 c0"))
        .unwrap();
        assert_eq!(keyboard_report(&d, &[1, 0xff, 1]).keys, BTreeSet::from([4, 6]));
    }

    #[test]
    fn only_supported_button_numbers_are_listed_or_reported() {
        let d =
            Descriptor::parse(&hex("05 01 09 02 a1 01 05 09 19 00 2a ff ff 15 00 27 ff ff 00 00 75 10 95 03 81 00 c0"))
                .unwrap();
        assert_eq!(d.buttons(), (1..=MAX_BUTTON).collect::<Vec<_>>());
        let m = mouse_report(&d, &[1, 0, MAX_BUTTON as u8, 0, 0xff, 0xff]);
        assert_eq!(m.buttons, BTreeSet::from([1, MAX_BUTTON]));
    }

    #[test]
    fn truncated_descriptors_and_random_untrusted_bytes_do_not_panic() {
        let bytes = keyboard();
        for end in 0..bytes.len() {
            if let Ok(d) = Descriptor::parse(&bytes[..end]) {
                assert!(!d.keyboard());
            }
        }
        // Deterministic bytes exercise short/long lengths and numeric tags.
        let mut state = 0x9e3779b9_u32;
        for len in 0..256 {
            let bytes: Vec<_> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state as u8
                })
                .collect();
            if let Ok(d) = Descriptor::parse(&bytes) {
                let _ = d.input(&bytes);
            }
        }
    }
}

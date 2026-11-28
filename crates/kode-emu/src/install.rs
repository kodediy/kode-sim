//! An app into the kodeOS a running simulator serves, through its console.
//!
//! What KodeOS's console takes from a cable on the board (kode-sdk's
//! `install src="terminal"`): `install src="terminal" dst=app size= crc32=
//! run=on name=`, the image a chunk per `>|ack` once kodeOS says
//! `>|ready chunk=<n>` (after a `>|evicted` for each app it had to drop),
//! and its `>ok` once the CRC and the image check out; kodeOS then reboots
//! into the app. The name is the image's project name, so that an older
//! copy of the same app gives its place first.
//!
//! Each `kode-emu run` also leaves a note in the cache (running/<pid>.json)
//! saying where its serial port and its page are, and takes it away when it
//! stops, so `kode-emu install` finds the simulator.
//!
//! `install --mailbox` needs no console: it posts the image to the
//! simulator's `api/mailbox`, which writes it into app_pending of the flash
//! esp-emu runs and resets the chip (see [`crate::bridge::Bridge::mailbox`]).

use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const PING_TIMEOUT: Duration = Duration::from_secs(5);
const READY_TIMEOUT: Duration = Duration::from_secs(12);
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(30);
/// What opening the console drops of what it said before we asked: at most
/// this long, and this much.
const DRAIN_FOR: Duration = Duration::from_secs(1);
const DRAIN_MAX: usize = 1 << 20;
/// The last sector of app_pending is kodeOS's intent flag, not the image's:
/// kode-sdk's KAPPS_PENDING_INTENT_OFF, and KBSP_MAILBOX_IMAGE_MAX
/// (0x5FF000 of today's 6 MB mailbox).
pub const MAILBOX_INTENT_SECTOR: u32 = 0x1000;

/// AO_Log wipes a half-drawn prompt with `\r ESC[K` before a record: what
/// came before it on the line is discarded, prompt and all (kcli.py's ERASE).
const ERASE: &str = "\r\x1b[K";

#[derive(Debug, PartialEq, Eq)]
pub struct InstallError(pub String);

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---- the note ----------------------------------------------------------------

fn note_path() -> std::path::PathBuf {
    crate::emu::cache_dir().join("running").join(format!("{}.json", std::process::id()))
}

/// Say where this simulator's serial port and page are.
pub fn announce(serial: &str, url: &str) -> std::io::Result<()> {
    let path = note_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let note = serde_json::json!({"pid": std::process::id(), "serial": serial, "url": url});
    std::fs::write(path, note.to_string())
}

/// Take the note away: this simulator is stopping.
pub fn withdraw() {
    let _ = std::fs::remove_file(note_path());
}

/// Where a running simulator is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub pid: u32,
    pub serial: String,
    pub url: String,
}

/// The note of the simulator started last that is still alive in `dir` -
/// or, with `port`, of the one whose page is on that port. Notes left by a
/// simulator that died without saying so are cleared.
pub fn running_in(dir: &std::path::Path, port: Option<u16>) -> Option<Note> {
    let mut found: Vec<(std::time::SystemTime, Note)> = vec![];
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(j) =
            std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        else {
            continue;
        };
        let note = parse_note(&j).filter(alive);
        let Some(note) = note else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if port.is_none_or(|p| note.url.trim_end_matches('/').ends_with(&format!(":{p}"))) {
            let when = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            found.push((when, note));
        }
    }
    found.into_iter().max_by_key(|(when, _)| *when).map(|(_, n)| n)
}

/// The note of the simulator started last that is still alive (or the one
/// whose page is on `port`), from kode-emu's cache.
pub fn running(port: Option<u16>) -> Option<Note> {
    running_in(&crate::emu::cache_dir().join("running"), port)
}

fn parse_note(j: &serde_json::Value) -> Option<Note> {
    let pid = match &j["pid"] {
        serde_json::Value::Number(n) => n.as_u64()?,
        serde_json::Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    let text = |k: &str| j.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    Some(Note { pid: u32::try_from(pid).ok()?, serial: text("serial"), url: text("url") })
}

/// Is the process that left it there, and its serial port?
fn alive(n: &Note) -> bool {
    n.pid > 0 && unsafe { libc::kill(n.pid as i32, 0) } == 0 && std::path::Path::new(&n.serial).exists()
}

// ---- the conversation --------------------------------------------------------

/// A console line as kcli reads it: the prompt a record wiped, and colour, gone.
pub fn clean(raw: &str) -> String {
    let last = raw.rsplit(ERASE).next().unwrap_or("");
    strip_ansi(last).trim().to_string()
}

/// Without `ESC [ digits/semicolons letter` sequences.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            let mut ahead = chars.clone();
            ahead.next();
            let mut seq = 1;
            let mut done = false;
            for a in ahead {
                seq += 1;
                if a.is_ascii_digit() || a == ';' {
                    continue;
                }
                done = a.is_ascii_alphabetic();
                break;
            }
            if done {
                for _ in 0..seq {
                    chars.next();
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// What the conversation needs of a console.
pub trait Port {
    fn write(&mut self, data: &[u8]);
    /// The next line that is a response (`>...`), cleaned, or None at the
    /// deadline.
    fn record(&mut self, timeout: Duration) -> Option<String>;
}

/// The terminator (`>ok`/`>err...`), or with `want` the first record it accepts.
fn answer(port: &mut dyn Port, timeout: Duration, want: Option<&dyn Fn(&str) -> bool>) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while let Some(rec) = port.record(deadline.saturating_duration_since(Instant::now())) {
        if rec.starts_with(">err") || rec.starts_with(">ok") || want.is_some_and(|w| w(&rec)) {
            return Some(rec);
        }
    }
    None
}

/// Where a [`LinesPort`]'s bytes go: the console's input.
pub type ConsoleOut = Box<dyn FnMut(&[u8]) + Send>;

/// A console reached in-process: lines arrive through the [`mpsc::Sender`]
/// [`LinesPort::new`] hands back (from any thread), bytes go out through
/// `send`. The page's upload uses it: the simulator's console is the
/// bridge's, not a serial port.
pub struct LinesPort {
    send: ConsoleOut,
    lines: mpsc::Receiver<String>,
}

impl LinesPort {
    pub fn new(send: ConsoleOut) -> (LinesPort, mpsc::Sender<String>) {
        let (tx, rx) = mpsc::channel();
        (LinesPort { send, lines: rx }, tx)
    }
}

impl Port for LinesPort {
    fn write(&mut self, data: &[u8]) {
        (self.send)(data)
    }

    fn record(&mut self, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())?;
            let line = clean(&self.lines.recv_timeout(left).ok()?);
            if line.starts_with('>') {
                return Some(line);
            }
        }
    }
}

/// A serial port: the simulator's console as kcli reaches it.
pub struct SerialPort {
    fd: std::os::fd::OwnedFd,
    buf: Vec<u8>,
}

impl SerialPort {
    /// Open it raw, and drop whatever the console said before we asked.
    pub fn open(path: &str) -> std::io::Result<SerialPort> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let c = std::ffi::CString::new(path).map_err(std::io::Error::other)?;
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd.as_raw_fd(), &mut t) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(fd.as_raw_fd(), libc::TCSANOW, &t);
        }
        let mut port = SerialPort { fd, buf: vec![] };
        // Until it falls quiet for 200 ms - but a console that never does
        // (an app logging in a loop) gets a second and a megabyte, no more.
        let deadline = Instant::now() + DRAIN_FOR;
        let mut drained = 0;
        while drained < DRAIN_MAX && port.wait(libc::POLLIN, 200) {
            match port.read_some() {
                Some(0) | None => break,
                Some(n) => drained += n,
            }
            port.buf.clear();
            if Instant::now() >= deadline {
                break;
            }
        }
        port.buf.clear();
        Ok(port)
    }

    /// Is it ready for `events` within `ms`?
    fn wait(&self, events: libc::c_short, ms: i32) -> bool {
        use std::os::fd::AsRawFd;
        let mut p = libc::pollfd { fd: self.fd.as_raw_fd(), events, revents: 0 };
        unsafe { libc::poll(&mut p, 1, ms) > 0 }
    }

    /// What there is to read, onto the buffer: its length, None if nothing.
    fn read_some(&mut self) -> Option<usize> {
        use std::os::fd::AsRawFd;
        let mut chunk = vec![0u8; 65536];
        let n = unsafe { libc::read(self.fd.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
        if n < 0 {
            return None;
        }
        self.buf.extend_from_slice(&chunk[..n as usize]);
        Some(n as usize)
    }
}

impl Port for SerialPort {
    fn write(&mut self, mut data: &[u8]) {
        use std::os::fd::AsRawFd;
        while !data.is_empty() {
            self.wait(libc::POLLOUT, 1000);
            let n = unsafe { libc::write(self.fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
            if n > 0 {
                data = &data[n as usize..];
            } else if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock {
                return;
            }
        }
    }

    fn record(&mut self, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = self.buf.drain(..=nl).collect();
                let line = clean(&String::from_utf8_lossy(&raw[..raw.len() - 1]));
                if line.starts_with('>') {
                    return Some(line);
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            if self.wait(libc::POLLIN, left.as_millis().clamp(1, i32::MAX as u128) as i32) {
                self.read_some();
            }
        }
    }
}

/// Where in the flash the mailbox (app_pending) is, for an app of `len`
/// bytes, from the partition table.
pub fn mailbox_slot(table: &[u8], len: usize) -> Result<u32, String> {
    let parts = crate::build::partitions(table);
    let Some(p) = parts.iter().find(|p| p.name == "app_pending") else {
        return Err("the partition table has no app_pending partition, the mailbox kodeOS adopts apps from".into());
    };
    let room = p.size.saturating_sub(MAILBOX_INTENT_SECTOR) as usize;
    if len > room {
        return Err(format!(
            "the app is {len} bytes and app_pending holds {room} bytes (0x{room:x}): its last 4 KB are kodeOS's intent flag"
        ));
    }
    Ok(p.offset)
}

/// An HTTP/1.1 response: its status and its body.
pub fn http_reply(raw: &[u8]) -> Option<(u16, String)> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..end]).ok()?;
    let status = head.lines().next()?.strip_prefix("HTTP/1.")?.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, String::from_utf8_lossy(&raw[end + 4..]).into_owned()))
}

/// `HOST:PORT` of a page's `http://HOST:PORT/` URL.
pub fn page_addr(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let addr = rest.split('/').next()?;
    (!addr.is_empty()).then(|| addr.to_string())
}

/// POST `body` to `path` on the page at `addr`: the reply's status and body.
fn post(addr: &str, path: &str, body: &[u8], timeout: Duration) -> std::io::Result<(u16, String)> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr)?;
    s.set_read_timeout(Some(timeout))?;
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    let mut raw = vec![];
    s.read_to_end(&mut raw)?;
    http_reply(&raw).ok_or_else(|| std::io::Error::other("not an HTTP reply"))
}

/// How long the simulator may take to put an app in its mailbox.
const MAILBOX_TIMEOUT: Duration = Duration::from_secs(120);

/// The app into the running simulator's mailbox, through its page's
/// `api/mailbox`: the simulator writes it into app_pending of the flash
/// esp-emu is running and resets the chip.
fn to_mailbox(image: &[u8], name: &str, sim: &Note) -> i32 {
    eprintln!("kode-emu: putting {name} in the mailbox (app_pending) of the simulator at {}", sim.url);
    let Some(addr) = page_addr(&sim.url) else {
        eprintln!("kode-emu: {} is not a page kode-emu can reach", sim.url);
        return 1;
    };
    match post(&addr, "/api/mailbox", image, MAILBOX_TIMEOUT) {
        Ok((200, body)) => {
            let j: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let at = j["offset"].as_u64().unwrap_or(0);
            eprintln!(
                "kode-emu: {} bytes at 0x{at:x}; the Kode Dot is resetting, and kodeOS adopts the app as it boots",
                image.len()
            );
            0
        }
        Ok((404, _)) => {
            eprintln!(
                "kode-emu: the simulator at {} takes no mailbox installs (is it an older kode-emu run?)",
                sim.url
            );
            1
        }
        Ok((_, body)) => {
            let j: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            eprintln!("kode-emu: {}", j["error"].as_str().unwrap_or(body.as_str()));
            1
        }
        Err(e) => {
            eprintln!("kode-emu: could not reach the simulator at {}: {e}", sim.url);
            1
        }
    }
}

/// `kode-emu install`.
///
/// It finds the simulator before anything else, so nothing is built with
/// nowhere to install it. PROJECT is a project to build or, a file, an app
/// image already built.
pub fn command(
    project: &std::path::Path,
    sdk: Option<&std::path::Path>,
    port: Option<u16>,
    mailbox: bool,
    board: bool,
) -> i32 {
    let Some(sim) = running(port) else {
        let which = port.map(|p| format!("on port {p}")).unwrap_or_else(|| "running".into());
        eprintln!(
            "kode-emu: no simulator is {which} - start kodeOS first (kode-emu run and the KodeOS folder), \
             then install into it from another terminal"
        );
        return 1;
    };
    let app = if project.is_file() {
        project.to_path_buf()
    } else {
        match crate::build::built(project, sdk, board) {
            Ok(fw) => fw.app_bin.expect("a build leaves its app"),
            Err(code) => return code,
        }
    };
    let image = match std::fs::read(&app) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("kode-emu: {}: {e}", app.display());
            return 1;
        }
    };
    if !is_app_image(&image) {
        eprintln!("kode-emu: {} is not an ESP-IDF app image (.bin)", app.display());
        return 1;
    }
    let name = app.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if mailbox {
        return to_mailbox(&image, &name, &sim);
    }
    eprintln!("kode-emu: installing {name} in the simulator at {}", sim.url);
    let mut console = match SerialPort::open(&sim.serial) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("kode-emu: could not open its serial port {}: {e}", sim.serial);
            return 1;
        }
    };
    let mut say = |s: &str| {
        if s.starts_with("->") {
            eprint!("\r{s}");
        } else {
            eprintln!("\r{s}");
        }
    };
    match install(&image, &mut console, &sim.serial, &mut say, PING_TIMEOUT) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("\nkode-emu: {e}");
            1
        }
    }
}

/// Is it an ESP-IDF app image: its header, starting with the image magic?
pub fn is_app_image(image: &[u8]) -> bool {
    image.len() >= 32 && image[0] == 0xE9
}

/// The project name in an ESP-IDF app image's app description
/// (esp_app_desc_t right after the image and first segment headers: its
/// magic, then project_name at 48), when it has one a console can carry.
pub fn project_name(image: &[u8]) -> Option<String> {
    const DESC: usize = 0x20;
    let magic = image.get(DESC..DESC + 4)?;
    if magic != 0xABCD_5432u32.to_le_bytes() {
        return None;
    }
    let raw = image.get(DESC + 48..DESC + 80)?;
    let name = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
    let name = std::str::from_utf8(name).ok()?;
    let plain = !name.is_empty() && name.chars().all(|c| c.is_ascii_graphic() && c != '"');
    plain.then(|| name.to_string())
}

/// zlib's CRC-32, which kodeOS checks the image against.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(data);
    crc.sum()
}

/// Install `image` over `port`, the console `name` names in errors; each
/// step goes to `say`. Blocking: run it off the async threads.
pub fn install(
    image: &[u8],
    port: &mut dyn Port,
    name: &str,
    say: &mut dyn FnMut(&str),
    ping_timeout: Duration,
) -> Result<(), InstallError> {
    port.write(b"ping\n");
    if answer(port, ping_timeout, None).as_deref() != Some(">ok") {
        return Err(InstallError(format!("no kodeOS console on {name} (is an app running instead?)")));
    }

    let mut line = format!("install src=\"terminal\" dst=app size={} crc32={} run=on", image.len(), crc32(image));
    if let Some(name) = project_name(image) {
        line.push_str(&format!(" name=\"{name}\""));
    }
    line.push('\n');
    port.write(line.as_bytes());
    let is_ready = |r: &str| r.starts_with(">|ready");
    let ready = answer(port, READY_TIMEOUT, Some(&is_ready));
    let ready = match ready {
        Some(r) if is_ready(&r) => r,
        other => return Err(InstallError(format!("install: {}", other.unwrap_or("kodeOS never said ready".into())))),
    };
    let step = ready
        .split_whitespace()
        .find_map(|w| w.strip_prefix("chunk=")?.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(4096);

    let t0 = Instant::now();
    let is_ack = |r: &str| r == ">|ack";
    for (i, piece) in image.chunks(step).enumerate() {
        port.write(piece);
        let done = (i * step + piece.len()).min(image.len());
        if piece.len() == step {
            let ack = answer(port, ACK_TIMEOUT, Some(&is_ack));
            if ack.as_deref() != Some(">|ack") {
                return Err(InstallError(format!(
                    "install: {} at {}/{} bytes",
                    ack.unwrap_or("no ack".into()),
                    i * step + step,
                    image.len()
                )));
            }
        }
        say(&format!("-> {done}/{} bytes", image.len()));
    }
    let done = answer(port, CLOSE_TIMEOUT, None);
    if done.as_deref() != Some(">ok") {
        return Err(InstallError(format!("install: {}", done.unwrap_or("no answer after the last byte".into()))));
    }
    say(&format!("installed {} bytes in {:.1}s; kodeOS is booting it", image.len(), t0.elapsed().as_secs_f64()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn a_record_behind_a_wiped_prompt_is_the_record() {
        assert_eq!(clean("kode> \r\x1b[K>|ready chunk=4096"), ">|ready chunk=4096");
        assert_eq!(clean("\x1b[0;32m>ok\x1b[0m  "), ">ok");
        assert_eq!(clean("I (12) boot: hi"), "I (12) boot: hi");
    }

    #[test]
    fn crc32_is_zlib_s() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
        assert_eq!(crc32(b"IEND"), 0xAE42_6082, "PNG's IEND chunk");
        // bit for bit the loop kode-emu had before
        let reference = |data: &[u8]| {
            let mut crc = !0u32;
            for &b in data {
                crc ^= b as u32;
                for _ in 0..8 {
                    crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
                }
            }
            !crc
        };
        for n in [1, 7, 64, 4095, 100_003] {
            let data = image(n);
            assert_eq!(crc32(&data), reference(&data), "{n} bytes");
        }
    }

    #[test]
    fn an_app_image_starts_with_esp_idf_s_magic() {
        assert!(is_app_image(&[&[0xE9u8][..], &[0; 31]].concat()));
        assert!(!is_app_image(&[0xE9; 31]), "shorter than an image header");
        assert!(!is_app_image(&[0; 64]));
        assert!(!is_app_image(b""));
    }

    #[test]
    fn a_lines_port_answers_only_records_and_gives_up_at_its_deadline() {
        let (mut port, feed) = LinesPort::new(Box::new(|_| {}));
        feed.send("I (1) chatter".into()).unwrap();
        feed.send("kode> \r\x1b[K>|pong".into()).unwrap();
        assert_eq!(port.record(Duration::from_millis(200)).as_deref(), Some(">|pong"));
        assert_eq!(port.record(Duration::from_millis(50)), None);
    }

    /// kodeOS's console as the fake esp-emu plays it: a pong, a ready
    /// behind a prompt, an ack per whole chunk, and the CRC checked.
    struct KodeOs {
        out: VecDeque<String>,
        pending: Vec<u8>,
        streaming: Option<(usize, u32, Vec<u8>)>,
        refuse: bool,
        silent: bool,
        /// The install line it was sent.
        asked: String,
    }

    impl KodeOs {
        fn new() -> Self {
            KodeOs {
                out: VecDeque::new(),
                pending: vec![],
                streaming: None,
                refuse: false,
                silent: false,
                asked: String::new(),
            }
        }
    }

    impl Port for KodeOs {
        fn write(&mut self, data: &[u8]) {
            if self.silent {
                return;
            }
            if let Some((size, crc, got)) = &mut self.streaming {
                let before = got.len() / 4096;
                got.extend_from_slice(data);
                for _ in before..got.len() / 4096 {
                    self.out.push_back(">|ack".into());
                }
                if got.len() >= *size {
                    let ok = crc32(got) == *crc && got.len() == *size;
                    self.out.push_back(if ok { ">ok".into() } else { ">err code=3 msg=crc".into() });
                    self.streaming = None;
                }
                return;
            }
            self.pending.extend_from_slice(data);
            while let Some(nl) = self.pending.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&self.pending[..nl]).to_string();
                self.pending.drain(..=nl);
                if line == "ping" {
                    self.out.extend(["kode> ping".into(), ">|pong".into(), ">ok".into()]);
                } else if let Some(args) = line.strip_prefix("install ") {
                    self.asked = line.clone();
                    if self.refuse {
                        self.out.push_back(">err code=5 msg=install".into());
                        continue;
                    }
                    let f = |k: &str| args.split_whitespace().find_map(|kv| kv.strip_prefix(k)).unwrap().to_string();
                    assert_eq!((f("src="), f("dst="), f("run=")), ("\"terminal\"".into(), "app".into(), "on".into()));
                    self.out.push_back(">|evicted name=\"old\" kept=off".into());
                    self.out.push_back("kode> \r\x1b[K>|ready chunk=4096".into());
                    self.streaming = Some((f("size=").parse().unwrap(), f("crc32=").parse().unwrap(), vec![]));
                }
            }
        }

        fn record(&mut self, _timeout: Duration) -> Option<String> {
            while let Some(l) = self.out.pop_front() {
                let l = clean(&l);
                if l.starts_with('>') {
                    return Some(l);
                }
            }
            None
        }
    }

    fn image(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + 3) as u8).collect()
    }

    #[test]
    fn install_streams_the_image_a_chunk_per_ack() {
        let mut said = vec![];
        let mut os = KodeOs::new();
        install(&image(10_000), &mut os, "x", &mut |s| said.push(s.to_string()), PING_TIMEOUT).unwrap();
        assert_eq!(said[..3], ["-> 4096/10000 bytes", "-> 8192/10000 bytes", "-> 10000/10000 bytes"]);
        assert!(said[3].starts_with("installed 10000 bytes in "), "{said:?}");
    }

    #[test]
    fn install_reports_kodeos_refusing() {
        let mut os = KodeOs::new();
        os.refuse = true;
        let e = install(&image(5000), &mut os, "x", &mut |_| {}, PING_TIMEOUT).unwrap_err();
        assert_eq!(e.0, "install: >err code=5 msg=install");
    }

    /// An ESP-IDF app image with `name` in its app description, as the
    /// build makes it: the image header, the first segment's, then
    /// esp_app_desc_t (magic, then project_name at 48).
    fn app_image(name: &str) -> Vec<u8> {
        let mut v = image(10_000);
        v[0x20..0x24].copy_from_slice(&0xABCD_5432u32.to_le_bytes());
        v[0x50..0x70].fill(0);
        v[0x50..0x50 + name.len()].copy_from_slice(name.as_bytes());
        v
    }

    #[test]
    fn install_names_the_project_so_its_old_copy_gives_its_place() {
        let mut os = KodeOs::new();
        let img = app_image("display_lvgl");
        install(&img, &mut os, "x", &mut |_| {}, PING_TIMEOUT).unwrap();
        assert_eq!(
            os.asked,
            format!("install src=\"terminal\" dst=app size=10000 crc32={} run=on name=\"display_lvgl\"", crc32(&img))
        );
        let mut os = KodeOs::new();
        install(&image(10_000), &mut os, "x", &mut |_| {}, PING_TIMEOUT).unwrap();
        assert!(!os.asked.contains("name="), "no app description, no name: {}", os.asked);
    }

    // ---- the notes ------------------------------------------------------------

    fn note(dir: &std::path::Path, pid: u32, serial: &str, url: &str, age_s: u64) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let f = dir.join(format!("{pid}.json"));
        std::fs::write(&f, serde_json::json!({"pid": pid, "serial": serial, "url": url}).to_string()).unwrap();
        let t = std::time::SystemTime::now() - Duration::from_secs(age_s);
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(t).unwrap();
        f
    }

    fn parent_pid() -> u32 {
        unsafe { libc::getppid() as u32 }
    }

    #[test]
    fn a_note_from_a_dead_simulator_is_not_a_simulator_and_goes() {
        let t = crate::build::tests::Tmp::new("notes");
        let dead = note(&t.0, 999_999, "/dev/null", "x", 0);
        assert_eq!(running_in(&t.0, None), None);
        assert!(!dead.exists());
        let no_port = note(&t.0, std::process::id(), "/nonexistent/tty", "x", 0);
        assert_eq!(running_in(&t.0, None), None);
        assert!(!no_port.exists());
    }

    #[test]
    fn the_newest_of_two_simulators_is_found_and_the_other_by_its_port() {
        let t = crate::build::tests::Tmp::new("notes");
        note(&t.0, parent_pid(), "/dev/null", "http://127.0.0.1:8765/", 60);
        let newer = note(&t.0, std::process::id(), "/dev/null", "http://127.0.0.1:8766/", 0);
        assert_eq!(running_in(&t.0, None).unwrap().url, "http://127.0.0.1:8766/");
        assert_eq!(running_in(&t.0, Some(8765)).unwrap().url, "http://127.0.0.1:8765/");
        assert_eq!(running_in(&t.0, Some(9999)), None);
        std::fs::remove_file(newer).unwrap();
        assert_eq!(running_in(&t.0, None).unwrap().url, "http://127.0.0.1:8765/");
        assert_eq!(running_in(&t.0.join("none"), None), None);
    }

    // ---- the serial port -------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn a_serial_port_with_nothing_behind_it_has_no_kodeos_console() {
        let s = crate::serial::Serial::open().unwrap();
        let e = tokio::task::block_in_place(|| {
            let mut port = SerialPort::open(&s.path).unwrap();
            install(&image(100), &mut port, &s.path, &mut |_| {}, Duration::from_millis(300)).unwrap_err()
        });
        assert_eq!(e.0, format!("no kodeOS console on {} (is an app running instead?)", s.path));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_serial_port_reads_records_line_by_line_and_writes_bytes() {
        let s = crate::serial::Serial::open().unwrap();
        s.write_line("chatter before we asked"); // drained on opening
        let mut port = tokio::task::block_in_place(|| SerialPort::open(&s.path)).unwrap();
        port.write(b"ping\n");
        assert_eq!(s.read().await.unwrap(), b"ping\n");
        s.write_line("I (1) boot");
        s.write_line("kode> \r\x1b[K>|pong");
        assert_eq!(port.record(Duration::from_secs(2)).as_deref(), Some(">|pong"));
        assert_eq!(port.record(Duration::from_millis(100)), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_chatty_console_does_not_keep_the_port_from_opening() {
        let s = std::sync::Arc::new(crate::serial::Serial::open().unwrap());
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let chatter = {
            let (s, stop) = (s.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    s.write_line("I (1) chatter that never stops");
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        let t0 = Instant::now();
        let port = tokio::task::block_in_place(|| SerialPort::open(&s.path));
        let took = t0.elapsed();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        chatter.join().unwrap();
        assert!(port.is_ok());
        assert!(took < Duration::from_secs(2), "opening took {took:?}");
    }

    // ---- the mailbox -------------------------------------------------------------

    #[test]
    fn the_mailbox_is_the_app_pending_partition() {
        let table = crate::build::tests::kode_sdk_table();
        assert_eq!(mailbox_slot(&table, 1000), Ok(0x1A00000));
        // kode-sdk's KBSP_MAILBOX_IMAGE_MAX: the last 4 KB are kodeOS's intent flag.
        assert_eq!(mailbox_slot(&table, 0x5FF000), Ok(0x1A00000));
        let e = mailbox_slot(&table, 0x5FF001).unwrap_err();
        assert!(e.contains("6287361 bytes") && e.contains("app_pending") && e.contains("6287360"), "{e}");
        assert!(e.contains("intent"), "{e}");
        assert!(mailbox_slot(&table, 0x600000).is_err(), "not over the intent sector");
        let none = crate::build::tests::partition_table(&[("factory", 0, 0, 0x20000, 0x400000)]);
        assert!(mailbox_slot(&none, 10).unwrap_err().contains("no app_pending partition"));
    }

    #[test]
    fn an_http_reply_is_its_status_and_its_body() {
        let r = b"HTTP/1.1 422 Unprocessable Entity\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":false}";
        assert_eq!(http_reply(r), Some((422, "{\"ok\":false}".to_string())));
        assert_eq!(http_reply(b"garbage"), None);
        assert_eq!(page_addr("http://127.0.0.1:8765/"), Some("127.0.0.1:8765".into()));
        assert_eq!(page_addr("ftp://x"), None);
    }

    #[test]
    fn install_with_nothing_listening_says_so() {
        let mut os = KodeOs::new();
        os.silent = true;
        let e = install(&image(100), &mut os, "the console", &mut |_| {}, Duration::from_millis(10)).unwrap_err();
        assert_eq!(e.0, "no kodeOS console on the console (is an app running instead?)");
    }
}

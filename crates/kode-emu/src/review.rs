//! `kode-emu review`: an app put through a scenario, headless, with a verdict.
//!
//! For apps.kode.diy's review: one emulator per review, kodeOS booted from
//! its own image, the app installed as a person would install it, a
//! scenario played (buttons, taps, console lines, waits, what the console
//! must say, screenshots), and a verdict:
//!
//! ```text
//! <out>/verdict.json   {"ok", "checks": [{"name", "ok", "detail"}],
//!                       "screenshots": [...], "log": "console.log", "duration_s"}
//! <out>/console.log    everything the Kode Dot said
//! <out>/*.png          the screen, 410x502
//! ```
//!
//! The exit code is 0 when every check passed, 1 when one failed, and 2
//! when the review could not run at all (not an app image, a bad scenario,
//! an out folder it cannot write) - so CI can tell a failing app from a
//! failing review.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::bridge::{truthy, Bridge, Config, H, W};
use crate::emu::Launch;
use crate::proto;

/// The review could not run: say why, and exit 2.
#[derive(Debug, PartialEq, Eq)]
pub struct ReviewError(pub String);

impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// In every image kode-emu builds; never in one built for the board.
const SIM_MARK: &[u8] = b"kemu_rx";
const BUTTONS: [(&str, u8); 7] = [
    ("top", kodedot_chips::BTN_TOP),
    ("bottom", kodedot_chips::BTN_BOT),
    ("bot", kodedot_chips::BTN_BOT),
    ("left", kodedot_chips::BTN_LEFT),
    ("right", kodedot_chips::BTN_RIGHT),
    ("up", kodedot_chips::BTN_UP),
    ("down", kodedot_chips::BTN_DOWN),
];
const SCENARIO_KEYS: [&str; 8] = ["timeout_s", "settle_s", "network", "wifi", "sd", "home_hold", "forbid", "steps"];
/// Each step's kind, in the order a step is recognised by, and the other
/// keys it may have.
const STEP_KEYS: [(&str, &[&str]); 6] = [
    ("wait", &[]),
    ("button", &["hold_ms"]),
    ("tap", &[]),
    ("console", &[]),
    ("expect_console", &["timeout_s"]),
    ("screenshot", &[]),
];

/// The screen - RGB565, little-endian, as the firmware sends it - as a PNG.
pub fn png(fb: &[u8], w: usize, h: usize) -> Vec<u8> {
    use std::io::Write;
    let mut rows = Vec::with_capacity(h * (1 + w * 3));
    for y in 0..h {
        rows.push(0); // no filter
        for x in 0..w {
            let at = (y * w + x) * 2;
            let v = u16::from_le_bytes([fb[at], fb[at + 1]]);
            let (r, g, b) = ((v >> 11) as u8, ((v >> 5) & 63) as u8, (v & 31) as u8);
            rows.extend_from_slice(&[(r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2)]);
        }
    }
    let mut z = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::new(6));
    z.write_all(&rows).expect("compressing into memory");
    let idat = z.finish().expect("compressing into memory");

    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crate::install::crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    let mut ihdr = vec![];
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8 bits, RGB
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    out
}

pub fn check_image(image: &[u8]) -> Result<(), ReviewError> {
    if !crate::install::is_app_image(image) {
        return Err(ReviewError("not an ESP-IDF app image (.bin)".into()));
    }
    if !image.windows(SIM_MARK.len()).any(|w| w == SIM_MARK) {
        return Err(ReviewError(
            "an app built for the board, not for the simulator: build it with `kode-emu build` and review \
             build/…/out/<app>.bin"
                .into(),
        ));
    }
    Ok(())
}

/// A value as Python's str() would show it, near enough: strings bare.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A scenario's regular expression, as Python's `re` reads it near enough
/// (fancy-regex): lookarounds and backreferences, and a `{` that starts no
/// `{m}`, `{m,}`, `{,n}` or `{m,n}` is a `{`. See the README for the rest.
pub type Pattern = fancy_regex::Regex;

fn pattern(v: &Value) -> Result<(String, Pattern), ReviewError> {
    let not = |why: String| ReviewError(format!("{v} is not a regular expression: {why}"));
    let p = v.as_str().ok_or_else(|| not("not a string".into()))?;
    let re = Pattern::new(p).map_err(|e| not(e.to_string().lines().last().unwrap_or("").trim().to_string()))?;
    Ok((p.to_string(), re))
}

/// Does `re` find something in `line`? A line that takes it past its
/// backtracking limit is a line it did not find anything in.
fn matches(re: &Pattern, line: &str) -> bool {
    re.is_match(line).unwrap_or(false)
}

/// The longest any time in a scenario may be, in seconds: a day.
pub const MAX_S: f64 = 86_400.0;
/// The longest a button may be held, in ms.
pub const MAX_HOLD_MS: f64 = 60_000.0;
/// The longest a screenshot's name may be.
const MAX_NAME: usize = 100;

/// A number of seconds (or ms) from 0 to `max`, or why not.
fn in_range(v: &Value, max: f64, what: &str) -> Result<f64, String> {
    match v.as_f64() {
        Some(x) if x.is_finite() && (0.0..=max).contains(&x) => Ok(x),
        Some(_) => Err(format!("{what} are from 0 to {} ({v})", secs(max))),
        None => Err(format!("{what} are a number ({v})")),
    }
}

/// A screenshot's name: a file in the out folder, nowhere else.
fn shot_name(v: &Value) -> Result<String, String> {
    let name = text(v);
    let plain = !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.contains(['/', '\\', '\0'])
        && !name.contains("..")
        && name != "."
        && !name.chars().any(char::is_control);
    if plain {
        Ok(name)
    } else {
        Err(format!("a screenshot's name is a plain file name of up to {MAX_NAME} characters, no /, \\ or .. ({v})"))
    }
}

/// One step of a scenario.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Wait(f64),
    Button { mask: u8, hold_s: f64 },
    Tap(u16, u16),
    Console(String),
    ExpectConsole { pattern: String, timeout_s: f64 },
    Screenshot(String),
}

/// A scenario with its defaults.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub timeout_s: f64,
    pub settle_s: f64,
    /// "none": nothing out but DNS; "internet".
    pub network: String,
    pub wifi: bool,
    pub sd: bool,
    pub home_hold: bool,
    /// Each pattern, and what it says.
    pub forbid: Vec<(String, Pattern)>,
    pub steps: Vec<Step>,
}

/// The scenario with its defaults, or what is wrong with it.
pub fn check_scenario(s: &Value) -> Result<Scenario, ReviewError> {
    let err = |m: String| Err(ReviewError(m));
    let Some(s) = s.as_object() else {
        return err("the scenario is a JSON object".into());
    };
    let mut unknown: Vec<&str> = s.keys().map(String::as_str).filter(|k| !SCENARIO_KEYS.contains(k)).collect();
    if !unknown.is_empty() {
        unknown.sort();
        return err(format!("the scenario has keys it does not know: {}", unknown.join(", ")));
    }
    let seconds = |k: &str, default: f64| match s.get(k) {
        None => Ok(default),
        Some(v) if v.is_number() => in_range(v, MAX_S, &format!("\"{k}\"'s seconds")).map_err(ReviewError),
        Some(v) => Err(ReviewError(format!("\"{k}\" is a number of seconds ({v})"))),
    };
    let flag = |k: &str| s.get(k).is_none_or(truthy);
    let network = match s.get("network") {
        None => "none".to_string(),
        Some(Value::String(n)) if n == "none" || n == "internet" => n.clone(),
        Some(_) => return err("\"network\" is \"none\" or \"internet\"".into()),
    };
    let list = |k: &str| match s.get(k) {
        None => Ok(vec![]),
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(v) => Err(ReviewError(format!("\"{k}\" is a list ({v})"))),
    };
    let kinds: Vec<&str> = STEP_KEYS.iter().map(|(k, _)| *k).collect();
    let mut steps = vec![];
    for (i, step) in list("steps")?.iter().enumerate() {
        let n = i + 1;
        let o = step.as_object();
        let kind = STEP_KEYS.iter().find(|(k, _)| o.is_some_and(|o| o.contains_key(*k)));
        let Some((kind, _)) =
            kind.filter(|(k, others)| o.unwrap().keys().all(|key| key == k || others.contains(&key.as_str())))
        else {
            return err(format!("step {n} is not one of: {} ({step})", kinds.join(", ")));
        };
        let o = o.unwrap();
        let v = &o[*kind];
        let number = |v: &Value| in_range(v, MAX_S, "seconds").map_err(|e| ReviewError(format!("step {n}: {e}")));
        steps.push(match *kind {
            "wait" => Step::Wait(number(v)?),
            "button" => {
                let Some(&(_, mask)) = BUTTONS.iter().find(|(name, _)| v.as_str() == Some(*name)) else {
                    let names: Vec<&str> = BUTTONS.iter().map(|(b, _)| *b).collect();
                    return err(format!("step {n}: no button {v} ({})", names.join(", ")));
                };
                let hold_ms = match o.get("hold_ms") {
                    None => 150.0,
                    Some(h) => {
                        in_range(h, MAX_HOLD_MS, "hold_ms").map_err(|e| ReviewError(format!("step {n}: {e}")))?
                    }
                };
                Step::Button { mask, hold_s: hold_ms / 1000.0 }
            }
            "tap" => {
                let at: Vec<u64> =
                    v.as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
                match at[..] {
                    [x, y] if v.as_array().is_some_and(|a| a.len() == 2) && (x as usize) < W && (y as usize) < H => {
                        Step::Tap(x as u16, y as u16)
                    }
                    _ => return err(format!("step {n}: a tap is [x, y] on the {W}x{H} screen")),
                }
            }
            "console" => Step::Console(text(v)),
            "expect_console" => {
                let timeout_s = match o.get("timeout_s") {
                    None => 10.0,
                    Some(t) => number(t)?,
                };
                Step::ExpectConsole { pattern: pattern(v)?.0, timeout_s }
            }
            _ => Step::Screenshot(shot_name(v).map_err(|e| ReviewError(format!("step {n}: {e}")))?),
        });
    }
    let forbid = list("forbid")?.iter().map(pattern).collect::<Result<Vec<_>, _>>()?;
    Ok(Scenario {
        timeout_s: seconds("timeout_s", 300.0)?,
        settle_s: seconds("settle_s", 5.0)?,
        network,
        wifi: flag("wifi"),
        sd: flag("sd"),
        home_hold: flag("home_hold"),
        forbid,
        steps,
    })
}

// ---- the run ----------------------------------------------------------------

/// The project name [`PROJECT`] found in `line`, from whichever of its forms.
fn project_of(re: &Regex, line: &str) -> Option<String> {
    let c = re.captures(line)?;
    (1..c.len()).find_map(|i| c.get(i)).map(|m| m.as_str().to_string())
}

/// What a crash looks like on the console: a panic, a watchdog, an abort, a
/// failed assert, QP's error handler.
const CRASH: &str = r"task_wdt|Guru Meditation|abort\(\)|Firmware abort|assertion|Q_onError|failed assert";
/// The firmware a boot runs, by its project name, in whichever line says it
/// first: ESP-IDF's app_init (at info level, which KodeOS keeps quiet),
/// kode-sdk's boot line ("kode kodeos 0.1.1 (...), the OS" / "kode audio 1
/// (...), an app"), or esp-emu's loader ("App kodeos 0.1.1 from flash").
const PROJECT: &str =
    r"Project name:\s+(\S+)|\bkode (\S+) \S+ \(.*\), (?:the OS|an app)|\bApp (\S+) \S+ from flash offset";
const KODEOS: &str = "kodeos";
/// kodeOS up and answering its console.
const BOOT_S: f64 = 60.0;
/// The app booted after its install.
const START_S: f64 = 60.0;
/// BOT held: past the SDK's CONFIG_KODEOS_HOME_HOLD (600 ms).
const HOME_HOLD_S: f64 = 1.0;
/// A tap: down this long, then up.
const TAP_S: f64 = 0.1;
const CARD_MB: u64 = 64;

/// Seconds as Python's `{t:g}` shows them, near enough: 10, 1.5.
fn secs(t: f64) -> String {
    if t.fract() == 0.0 && t.abs() < 1e15 {
        format!("{}", t as i64)
    } else {
        format!("{t}")
    }
}

/// `s` seconds, the longest a Duration holds if more, none if negative.
fn duration(s: f64) -> Duration {
    Duration::try_from_secs_f64(s.max(0.0)).unwrap_or(Duration::MAX)
}

/// `s` seconds from now - or as far on as the clock goes.
fn after(s: f64) -> Instant {
    let now = Instant::now();
    now.checked_add(duration(s)).or_else(|| now.checked_add(Duration::from_secs(100 * 365 * 86_400))).unwrap_or(now)
}

fn sleep(s: f64) -> tokio::time::Sleep {
    tokio::time::sleep(duration(s))
}

/// How much longer than its own time a hold may take on the wall when the
/// firmware's clock does not reach it (a chip that stopped).
const HOLD_SLACK: Duration = Duration::from_secs(30);

/// `d` on the firmware's clock - `clock`, its emulated nanoseconds, which a
/// slow Mac runs slower than the wall's - or on the wall's, without one.
/// Never more than `slack` past `d` on the wall. The clock starts again at
/// a chip reset (kodeOS going home reboots): what it ran is summed, step by
/// step, a step back counting from where it went.
async fn hold(clock: &dyn Fn() -> Option<u64>, d: Duration, slack: Duration) {
    let wall_end = Instant::now() + d;
    let Some(mut last) = clock() else {
        tokio::time::sleep(d).await;
        return;
    };
    let need = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    let give_up = wall_end + slack;
    let mut ran: u64 = 0;
    while ran < need && Instant::now() < give_up {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let Some(now) = clock() else { break };
        ran = ran.saturating_add(if now >= last { now - last } else { now });
        last = now;
    }
}

/// The Kode Dot's emulated time, in ns, for holds the firmware sees as long
/// as they are meant (kodeOS's 600 ms home hold, a scenario's hold_ms).
///
/// None before the chips have heard from the emulator: then the wall's.
fn sim_now(bridge: &Bridge) -> Option<u64> {
    bridge.sim_time_ns().filter(|&t| t > 0)
}

/// A review's own folder (its flash, its card): gone when this is dropped,
/// however the review ends.
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(path: PathBuf) -> std::io::Result<TempDir> {
        std::fs::create_dir_all(&path)?;
        Ok(TempDir(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// SIGTERM (a CI's timeout), SIGINT (Ctrl-C) or SIGHUP (a closed terminal),
/// listened for from the moment this is called: the one that came.
fn stop_signal() -> impl std::future::Future<Output = i32> {
    use tokio::signal::unix::{signal, SignalKind};
    let kinds = [
        (SignalKind::terminate(), libc::SIGTERM),
        (SignalKind::interrupt(), libc::SIGINT),
        (SignalKind::hangup(), libc::SIGHUP),
    ];
    let mut signals: Vec<_> = kinds.into_iter().filter_map(|(k, n)| signal(k).ok().map(|s| (s, n))).collect();
    async move {
        if signals.is_empty() {
            return std::future::pending().await;
        }
        let each = signals.iter_mut().map(|(s, n)| {
            let n = *n;
            Box::pin(async move {
                s.recv().await;
                n
            })
        });
        futures_util::future::select_all(each).await.0
    }
}

struct Run {
    bridge: Arc<Bridge>,
    out: PathBuf,
    /// Everything the Kode Dot said, with when (seconds since the start).
    lines: Arc<Mutex<Vec<(f64, String)>>>,
    news: Arc<Notify>,
    checks: Vec<Value>,
    shots: Vec<String>,
    project: Regex,
}

impl Run {
    fn check(&mut self, name: &str, ok: bool, detail: &str) -> bool {
        self.checks.push(json!({"name": name, "ok": ok, "detail": detail}));
        ok
    }

    fn running(&self) -> bool {
        self.bridge.state().0 == "running"
    }

    fn said(&self) -> usize {
        self.lines.lock().unwrap().len()
    }

    fn shoot(&mut self, name: &str) {
        let file = format!("{name}.png");
        if let Err(e) = std::fs::write(self.out.join(&file), png(&self.bridge.framebuffer(), W, H)) {
            eprintln!("kode-emu review: could not write {}: {e}", self.out.join(&file).display());
            return;
        }
        if !self.shots.contains(&file) {
            self.shots.push(file);
        }
    }

    /// What `pred` makes of the first line from `since` on that it likes, or
    /// None by `timeout` or when the Kode Dot stops.
    async fn until(&self, pred: impl Fn(&str) -> Option<String>, timeout: f64, since: usize) -> Option<String> {
        let end = after(timeout);
        let mut seen = since;
        loop {
            let fresh: Vec<String> = {
                let lines = self.lines.lock().unwrap();
                lines.get(seen..).map(|l| l.iter().map(|(_, s)| s.clone()).collect()).unwrap_or_default()
            };
            for line in fresh {
                seen += 1;
                if let Some(got) = pred(&line) {
                    return Some(got);
                }
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() || !self.running() {
                return None;
            }
            let _ = tokio::time::timeout(left.min(Duration::from_millis(500)), self.news.notified()).await;
        }
    }

    /// Has the screen something on it, by `timeout`?
    async fn drawn(&self, timeout: f64) -> bool {
        let end = after(timeout);
        while self.bridge.framebuffer().iter().all(|&b| b == 0) {
            if Instant::now() > end || !self.running() {
                return false;
            }
            sleep(0.2).await;
        }
        true
    }

    /// Held for `s` as the firmware counts time (see [`sim_now`]).
    async fn held(&self, s: f64) {
        let b = self.bridge.clone();
        hold(&move || sim_now(&b), duration(s), HOLD_SLACK).await;
    }

    async fn buttons(&self, mask: u8, hold_s: f64) {
        self.bridge.from_page(proto::BUTTONS, &[mask]).await;
        self.held(hold_s).await;
        self.bridge.from_page(proto::BUTTONS, &[0]).await;
    }

    async fn tap(&self, x: u16, y: u16) {
        let touch = |pressed: u8| {
            let mut p = vec![pressed];
            p.extend_from_slice(&x.to_le_bytes());
            p.extend_from_slice(&y.to_le_bytes());
            p
        };
        self.bridge.from_page(proto::TOUCH, &touch(1)).await;
        self.held(TAP_S).await;
        self.bridge.from_page(proto::TOUCH, &touch(0)).await;
    }

    fn app_named(&self, line: &str) -> Option<String> {
        project_of(&self.project, line)
    }
}

async fn play(r: &mut Run, image: &[u8], s: &Scenario) {
    let b = r.bridge.clone();
    // kodeOS up, answering its console as kcli would find it.
    let mut up = None;
    let end = after(BOOT_S);
    while up.is_none() && Instant::now() < end && r.running() {
        let mark = r.said();
        b.console_in("ping");
        up = r.until(|l| l.starts_with(">ok").then(String::new), 2.0, mark).await;
    }
    let drawn = up.is_some() && r.drawn(end.saturating_duration_since(Instant::now()).as_secs_f64().max(1.0)).await;
    let why = if drawn {
        String::new()
    } else if up.is_none() {
        format!("no answer to ping in {}s", secs(BOOT_S))
    } else {
        "its screen stayed dark".into()
    };
    if !r.check("kodeos_boots", drawn, &why) {
        return;
    }
    sleep(2.0).await; // the UI's first frames, past its first rows
    r.shoot("kodeos");

    let mark = r.said();
    if let Err(e) = b.install_image(image.to_vec()).await {
        r.check("installs", false, &e.0);
        return;
    }
    r.check("installs", true, &format!("{} bytes", image.len()));
    let project = r.project.clone();
    let name = r.until(|l| project_of(&project, l).filter(|n| n != KODEOS), START_S, mark).await;
    let detail = match &name {
        Some(n) => format!("booted {n}"),
        None => format!("no app booted in {}s", secs(START_S)),
    };
    if !r.check("app_starts", name.is_some(), &detail) {
        return;
    }
    sleep(s.settle_s).await;
    r.shoot("app");

    for step in &s.steps {
        if !r.running() {
            return;
        }
        match step {
            Step::Wait(t) => sleep(*t).await,
            Step::Button { mask, hold_s } => r.buttons(*mask, *hold_s).await,
            Step::Tap(x, y) => r.tap(*x, *y).await,
            Step::Console(line) => b.console_in(line),
            Step::ExpectConsole { pattern, timeout_s } => {
                let re = Pattern::new(pattern).expect("checked with the scenario");
                // said at any time so far
                let got = r.until(|l| matches(&re, l).then(|| l.to_string()), *timeout_s, 0).await;
                let detail = got.clone().unwrap_or_else(|| format!("not said within {}s", secs(*timeout_s)));
                if !r.check(&format!("expect_console: {pattern}"), got.is_some(), &detail) {
                    return;
                }
            }
            Step::Screenshot(name) => r.shoot(name),
        }
    }

    if s.home_hold && r.running() {
        let mark = r.said();
        r.buttons(kodedot_chips::BTN_BOT, HOME_HOLD_S).await;
        let home = r.until(|l| r.app_named(l).filter(|n| n == KODEOS), 30.0, mark).await;
        r.check(
            "home_by_hold",
            home.is_some(),
            if home.is_some() { "" } else { "BOT held 1 s did not bring kodeOS back" },
        );
        if home.is_some() {
            sleep(2.0).await;
            r.shoot("home");
        }
    }
}

/// kodeOS for the simulator: its image and its ELF.
pub struct KodeOs {
    pub merged_bin: PathBuf,
    pub elf: PathBuf,
}

/// How a review that gave no verdict ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// It could not run (exit 2).
    Cannot(ReviewError),
    /// A signal stopped it, this one: the emulator stopped and its files
    /// gone (exit 128 + it, as a shell has it).
    Stopped(i32),
}

impl From<ReviewError> for Ended {
    fn from(e: ReviewError) -> Ended {
        Ended::Cannot(e)
    }
}

/// Put `image` through `scenario` in a Kode Dot of its own; the verdict,
/// also written to `out` with the console log and the screenshots. A
/// SIGTERM, SIGINT or SIGHUP on the way stops the emulator and takes the
/// review's files away.
pub async fn review(image: &[u8], kodeos: &KodeOs, scenario: &Value, out: &Path) -> Result<Value, Ended> {
    check_image(image)?;
    let s = check_scenario(scenario)?;
    let cannot = |e: std::io::Error| ReviewError(format!("cannot write {}: {e}", out.display()));
    std::fs::create_dir_all(out).map_err(cannot)?;
    let probe = out.join(".probe");
    std::fs::write(&probe, b"").map_err(cannot)?;
    std::fs::remove_file(&probe).map_err(cannot)?;
    let stop = stop_signal();
    let t0 = Instant::now();
    let tmp = std::env::temp_dir().join(format!(
        "kode-emu-review-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()
    ));
    let tmp = TempDir::new(tmp.clone()).map_err(|e| ReviewError(format!("could not make {}: {e}", tmp.display())))?;
    let running: Mutex<Option<Arc<Bridge>>> = Mutex::new(None);
    tokio::select! {
        verdict = review_in(tmp.path(), image, kodeos, &s, out, t0, &running) => Ok(verdict?),
        sig = stop => {
            // review_in is dropped by now: a launch under way has stopped its esp-emu.
            let bridge = running.lock().unwrap().take();
            if let Some(b) = bridge {
                b.close().await;
            }
            Err(Ended::Stopped(sig))
        }
    }
}

async fn review_in(
    tmp: &Path,
    image: &[u8],
    kodeos: &KodeOs,
    s: &Scenario,
    out: &Path,
    t0: Instant,
    running: &Mutex<Option<Arc<Bridge>>>,
) -> Result<Value, ReviewError> {
    let flash = tmp.join("flash.bin");
    std::fs::copy(&kodeos.merged_bin, &flash)
        .map_err(|e| ReviewError(format!("{}: {e}", kodeos.merged_bin.display())))?;
    let card = if s.sd {
        let card = tmp.join("card.img");
        crate::sdcard::make_card(&card, CARD_MB, crate::sdcard::LABEL).map_err(ReviewError)?;
        Some(card)
    } else {
        None
    };
    let bridge = Bridge::new(Config {
        launch: Launch {
            firmware: flash,
            elf: Some(kodeos.elf.clone()),
            forwards: vec![],
            restrict: s.network == "none",
            save_state: false,
            sd: None,
            wifi: s.wifi,
        },
        http_port: 0,
        sd: card,
        sd_in: true,
    });
    *running.lock().unwrap() = Some(bridge.clone());
    let lines: Arc<Mutex<Vec<(f64, String)>>> = Arc::default();
    let news = Arc::new(Notify::new());
    {
        let (lines, news) = (lines.clone(), news.clone());
        bridge.on_line(Box::new(move |l| {
            lines.lock().unwrap().push((t0.elapsed().as_secs_f64(), l.to_string()));
            news.notify_one();
        }));
    }
    let (state_tx, mut states) = tokio::sync::mpsc::unbounded_channel();
    bridge.on_state(state_tx);
    {
        let news = news.clone();
        tokio::spawn(async move {
            while states.recv().await.is_some() {
                news.notify_one();
            }
        });
    }
    let mut r = Run {
        bridge: bridge.clone(),
        out: out.to_path_buf(),
        lines,
        news,
        checks: vec![],
        shots: vec![],
        project: Regex::new(PROJECT).unwrap(),
    };
    if let Err(e) = bridge.start_headless().await {
        bridge.close().await;
        return Err(ReviewError(e));
    }
    if tokio::time::timeout(duration(s.timeout_s), play(&mut r, image, s)).await.is_err() {
        r.check("in_time", false, &format!("the scenario took more than {}s", secs(s.timeout_s)));
    }
    if r.running() {
        sleep(0.5).await; // what it says as it goes down is part of the log
    }
    let failed = r.checks.iter().any(|c| c["ok"] != true);
    if failed && bridge.framebuffer().iter().any(|&b| b != 0) {
        r.shoot("failed");
    }
    bridge.close().await;

    let said: Vec<(f64, String)> = r.lines.lock().unwrap().clone();
    let crash = Regex::new(CRASH).unwrap();
    let mut crashes: Vec<String> = said.iter().filter(|(_, l)| crash.is_match(l)).map(|(_, l)| l.clone()).collect();
    let (state, detail) = bridge.state();
    if state == "crashed" {
        crashes.push(detail.lines().next().unwrap_or("").to_string());
    }
    let first: Vec<String> = crashes.iter().take(10).cloned().collect();
    r.check("no_crash", crashes.is_empty(), &first.join("\n"));
    for (pat, re) in &s.forbid {
        let hits: Vec<String> = said.iter().filter(|(_, l)| matches(re, l)).take(5).map(|(_, l)| l.clone()).collect();
        r.check(&format!("forbid: {pat}"), hits.is_empty(), &hits.join("\n"));
    }
    let log: String = said.iter().map(|(t, l)| format!("{t:9.3} {l}\n")).collect();
    std::fs::write(out.join("console.log"), log)
        .map_err(|e| ReviewError(format!("cannot write {}: {e}", out.display())))?;
    let ok = r.checks.iter().all(|c| c["ok"] == true);
    let verdict = json!({
        "ok": ok,
        "checks": r.checks,
        "screenshots": r.shots,
        "log": "console.log",
        "duration_s": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
    });
    let text = serde_json::to_string_pretty(&verdict).expect("a verdict is JSON") + "\n";
    std::fs::write(out.join("verdict.json"), text)
        .map_err(|e| ReviewError(format!("cannot write {}: {e}", out.display())))?;
    Ok(verdict)
}

/// kodeOS for the simulator: built files (merged-binary.bin and
/// kodeos.elf), or a KodeOS checkout to build.
fn kodeos(path: &Path) -> Result<KodeOs, ReviewError> {
    let (merged_bin, elf) = (path.join("merged-binary.bin"), path.join("kodeos.elf"));
    if merged_bin.exists() && elf.exists() {
        return Ok(KodeOs { merged_bin, elf });
    }
    eprintln!("kode-emu: building {} ...", path.display());
    let fw = crate::build::build(path, None, false).map_err(|e| ReviewError(e.0))?;
    Ok(KodeOs { merged_bin: fw.merged_bin, elf: fw.elf })
}

/// `kode-emu review`: 0 when every check passed, 1 when one failed, 2 when
/// the review could not run.
pub async fn command(app: &Path, scenario: Option<&Path>, out: &Path, kodeos_at: &Path) -> i32 {
    let verdict = async {
        let io = |p: &Path, e: std::io::Error| ReviewError(format!("{}: {e}", p.display()));
        let scenario: Value = match scenario {
            Some(p) => {
                let text = std::fs::read_to_string(p).map_err(|e| io(p, e))?;
                serde_json::from_str(&text).map_err(|e| ReviewError(format!("{}: {e}", p.display())))?
            }
            None => json!({}),
        };
        let image = if app.is_dir() {
            eprintln!("kode-emu: building {} ...", app.display());
            let fw = crate::build::build(app, None, false).map_err(|e| ReviewError(e.0))?;
            let bin = fw.app_bin.expect("a build leaves its app");
            std::fs::read(&bin).map_err(|e| io(&bin, e))?
        } else {
            std::fs::read(app).map_err(|e| io(app, e))?
        };
        // What is wrong with the app or the scenario is said before kodeOS is built.
        check_image(&image)?;
        check_scenario(&scenario)?;
        review(&image, &kodeos(kodeos_at)?, &scenario, out).await
    }
    .await;
    let verdict = match verdict {
        Ok(v) => v,
        Err(Ended::Cannot(e)) => {
            eprintln!("kode-emu review: {e}");
            return 2;
        }
        Err(Ended::Stopped(sig)) => {
            eprintln!(
                "kode-emu review: stopped by signal {sig}: no verdict; the emulator is stopped and its files gone"
            );
            return 128 + sig;
        }
    };
    for c in verdict["checks"].as_array().into_iter().flatten() {
        let ok = c["ok"] == true;
        let detail = c["detail"].as_str().unwrap_or("");
        let why = if !ok && !detail.is_empty() {
            format!(": {}", detail.lines().next().unwrap_or(""))
        } else {
            String::new()
        };
        eprintln!("{} {}{why}", if ok { "ok  " } else { "FAIL" }, c["name"].as_str().unwrap_or(""));
    }
    let ok = verdict["ok"] == true;
    eprintln!(
        "kode-emu review: {} in {}s; {}",
        if ok { "passed" } else { "failed" },
        verdict["duration_s"],
        out.join("verdict.json").display()
    );
    if ok {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inflate(data: &[u8]) -> Vec<u8> {
        use std::io::Read;
        let mut out = vec![];
        flate2::read::ZlibDecoder::new(data).read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn a_boot_is_named_by_esp_idf_kode_sdk_or_esp_emu() {
        let re = Regex::new(PROJECT).unwrap();
        let name = |l: &str| project_of(&re, l);
        assert_eq!(name("I (629) app_init: Project name:     kodeos").as_deref(), Some("kodeos"));
        assert_eq!(
            name("!N 2026-11-13T15:09:59.127Z kode kodeos 0.1.1 (Nov 13 2026 15:23:26, ESP-IDF v6.1), the OS, from factory")
                .as_deref(),
            Some("kodeos")
        );
        assert_eq!(
            name("!N 2026-11-13T13:19:41.397Z kode audio 1 (Nov 13 2026 14:19:22, ESP-IDF v6.1), an app").as_deref(),
            Some("audio")
        );
        assert_eq!(
            name("[INFO  machine] App display_lvgl 1 from flash offset 0x20000: application ELF #1 is its build")
                .as_deref(),
            Some("display_lvgl")
        );
        assert_eq!(name("!N 2026-11-13T13:19:41.397Z kode sdk 0.1.1"), None);
        assert_eq!(name("!N kode hardware Kode Dot DVT, ESP32-P4 v3.1"), None);
    }

    #[test]
    fn a_screenshot_is_a_png_of_the_screen() {
        let (w, h) = (3, 2);
        let mut fb = vec![0u8; w * h * 2];
        fb[0..2].copy_from_slice(&0xF800u16.to_le_bytes()); // red
        fb[2..4].copy_from_slice(&0x07E0u16.to_le_bytes()); // green
        fb[8..10].copy_from_slice(&0xFFFFu16.to_le_bytes()); // white, second row
        let png = png(&fb, w, h);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let ihdr = &png[16..29];
        assert_eq!(&ihdr[..8], &[0, 0, 0, 3, 0, 0, 0, 2]);
        assert_eq!(&ihdr[8..], &[8, 2, 0, 0, 0]);
        let crc = u32::from_be_bytes(png[29..33].try_into().unwrap());
        assert_eq!(crc, crate::install::crc32(&png[12..29]), "IHDR's CRC covers its type and data");
        let at = png.windows(4).position(|x| x == b"IDAT").unwrap();
        let size = u32::from_be_bytes(png[at - 4..at].try_into().unwrap()) as usize;
        let rows = inflate(&png[at + 4..at + 4 + size]);
        assert_eq!(rows.len(), h * (1 + w * 3));
        assert_eq!(rows[0], 0);
        assert_eq!(&rows[1..7], &[255, 0, 0, 0, 255, 0]);
        assert_eq!(rows[1 + w * 3], 0);
        assert_eq!(&rows[2 + w * 3 + 3..2 + w * 3 + 6], &[255, 255, 255]);
        assert!(png.ends_with(b"IEND\xaeB`\x82"));
    }

    #[test]
    fn only_an_app_built_for_the_simulator_is_reviewed() {
        let mut app = vec![0xE9];
        app.extend_from_slice(&[0; 200]);
        app.extend_from_slice(SIM_MARK);
        assert_eq!(check_image(&app), Ok(()));
        assert!(check_image(b"not an app at all").unwrap_err().0.contains("not an ESP-IDF app image"));
        let board = [&[0xE9u8][..], &[0; 4000]].concat();
        assert!(check_image(&board).unwrap_err().0.contains("built for the board"));
    }

    fn bad(s: Value) -> String {
        check_scenario(&s).unwrap_err().0
    }

    #[test]
    fn a_scenario_has_its_defaults() {
        let s = check_scenario(&json!({})).unwrap();
        assert_eq!((s.timeout_s, s.settle_s), (300.0, 5.0));
        assert_eq!(s.network, "none");
        assert!(s.wifi && s.sd && s.home_hold);
        assert!(s.forbid.is_empty() && s.steps.is_empty());
    }

    #[test]
    fn a_scenario_s_steps_are_read() {
        let s = check_scenario(&json!({"settle_s": 0.5, "network": "internet", "wifi": false, "forbid": ["E \\("],
            "steps": [{"wait": 0.2}, {"button": "right", "hold_ms": 100}, {"button": "bot"}, {"tap": [10, 20]},
                      {"console": "hola"}, {"expect_console": "echo: hola", "timeout_s": 5}, {"expect_console": "x"},
                      {"screenshot": "after"}]}))
        .unwrap();
        assert_eq!(s.settle_s, 0.5);
        assert_eq!(s.network, "internet");
        assert!(!s.wifi);
        assert!(matches(&s.forbid[0].1, "E (12) oops") && s.forbid[0].0 == "E \\(");
        assert_eq!(
            s.steps,
            vec![
                Step::Wait(0.2),
                Step::Button { mask: kodedot_chips::BTN_RIGHT, hold_s: 0.1 },
                Step::Button { mask: kodedot_chips::BTN_BOT, hold_s: 0.15 },
                Step::Tap(10, 20),
                Step::Console("hola".into()),
                Step::ExpectConsole { pattern: "echo: hola".into(), timeout_s: 5.0 },
                Step::ExpectConsole { pattern: "x".into(), timeout_s: 10.0 },
                Step::Screenshot("after".into()),
            ]
        );
    }

    #[test]
    fn a_scenario_it_does_not_understand_is_refused() {
        assert!(bad(json!([])).contains("JSON object"));
        assert!(bad(json!({"timeout": 5})).contains("keys it does not know: timeout"));
        assert!(bad(json!({"network": "lan"})).contains("\"network\" is \"none\" or \"internet\""));
        assert!(bad(json!({"steps": [{"dance": true}]})).contains("step 1 is not one of: wait, button, tap"));
        assert!(bad(json!({"steps": [{"wait": 1}, {"wait": 1, "hold_ms": 3}]})).starts_with("step 2 is not one of"));
        assert!(bad(json!({"steps": [{"button": "middle"}]})).contains("step 1: no button \"middle\""));
        assert!(bad(json!({"steps": [{"tap": [410, 0]}]})).contains("a tap is [x, y] on the 410x502 screen"));
        assert!(bad(json!({"steps": [{"tap": [1]}]})).contains("a tap is [x, y]"));
        assert!(bad(json!({"steps": [{"wait": "a while"}]})).contains("seconds are a number"));
        assert!(bad(json!({"steps": [{"expect_console": "x", "timeout_s": "5"}]})).contains("seconds are a number"));
    }

    #[test]
    fn a_scenario_with_a_bad_pattern_is_refused() {
        assert!(bad(json!({"steps": [{"expect_console": "(unclosed"}]})).contains("is not a regular expression"));
        assert!(bad(json!({"forbid": ["[x"]})).contains("is not a regular expression"));
        assert!(bad(json!({"forbid": [3]})).contains("is not a regular expression"));
    }

    #[test]
    fn a_scenario_s_times_are_from_0_to_a_day() {
        for s in [
            json!({"timeout_s": 1e300}),
            json!({"settle_s": -1}),
            json!({"timeout_s": 86_400.5}),
            json!({"steps": [{"wait": 1e300}]}),
            json!({"steps": [{"wait": -0.5}]}),
            json!({"steps": [{"button": "top", "hold_ms": 1e12}]}),
            json!({"steps": [{"button": "top", "hold_ms": -1}]}),
            json!({"steps": [{"expect_console": "x", "timeout_s": 1e300}]}),
        ] {
            let e = bad(s.clone());
            assert!(e.contains("from 0 to"), "{s}: {e}");
        }
        let s = check_scenario(&json!({"timeout_s": 86_400, "settle_s": 0, "steps": [{"wait": 0},
            {"button": "top", "hold_ms": 60_000}]}))
        .unwrap();
        assert_eq!(s.timeout_s, 86_400.0);
    }

    #[test]
    fn times_too_long_for_a_clock_are_as_long_as_it_goes() {
        assert_eq!(duration(1e300), Duration::MAX);
        assert_eq!(duration(-1.0), Duration::ZERO);
        assert_eq!(duration(1.5), Duration::from_millis(1500));
        assert!(after(1e300) > Instant::now() + Duration::from_secs(86_400 * 365));
        assert!(after(-1.0) <= Instant::now());
    }

    #[test]
    fn a_screenshot_is_named_plainly() {
        for name in ["../evil", "/etc/x", "a/b", "a\\b", "..", ".", "", "a\u{0}b", "x..y", &"x".repeat(101)] {
            let e = bad(json!({"steps": [{"screenshot": name}]}));
            assert!(e.contains("step 1: a screenshot's name"), "{name:?}: {e}");
        }
        let s = check_scenario(&json!({"steps": [{"screenshot": "after-tap_2"}, {"screenshot": 3}]})).unwrap();
        assert_eq!(s.steps, vec![Step::Screenshot("after-tap_2".into()), Step::Screenshot("3".into())]);
    }

    /// Does the scenario's pattern find something in `line`?
    fn finds(p: &str, line: &str) -> bool {
        let s = check_scenario(&json!({"forbid": [p]})).unwrap_or_else(|e| panic!("{p}: {e}"));
        matches(&s.forbid[0].1, line)
    }

    #[test]
    fn patterns_are_python_s() {
        assert!(finds(r#"{"ok":true}"#, r#"reply {"ok":true} sent"#), "a brace that quantifies nothing is a brace");
        assert!(!finds(r#"{"ok":true}"#, r#"{"ok":false}"#));
        assert!(finds("{", "a { b"));
        assert!(finds("x{", "x{"));
        assert!(finds("^x{,2}$", "xx"), "{{,n}} is {{0,n}}");
        assert!(finds("^a{1,2$", "a{1,2"));
        assert!(finds("^x{2}$", "xx") && !finds("^x{2}$", "x"));
        assert!(finds("(?<=temp=)\\d+", "temp=21") && !finds("(?<=temp=)\\d+", "hum=21"), "lookbehind");
        assert!(finds("error(?! 0)", "error 1") && !finds("error(?! 0)", "error 0"), "negative lookahead");
        assert!(finds(r"(\w+) \1", "hola hola") && !finds(r"^(\w+) \1$", "hola adios"), "a backreference");
        assert!(finds(r"(?P<w>\w+)-(?P=w)", "ab-ab"), "a named backreference");
        assert!(finds("(?i)HOLA", "hola"));
        // Not Python's, as the README says: these are written escaped.
        assert!(finds(r"[\[\&\~]", "&") && finds(r"[\[\&\~]", "[") && finds(r"[\[\&\~]", "~"));
    }

    #[test]
    fn a_review_s_temp_dir_goes_with_it() {
        let dir = std::env::temp_dir().join(format!("kode-emu-test-tempdir-{}", std::process::id()));
        let tmp = TempDir::new(dir.clone()).unwrap();
        std::fs::write(tmp.path().join("flash.bin"), b"x").unwrap();
        assert!(dir.exists());
        drop(tmp);
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn a_hold_lasts_its_time_on_the_firmware_s_clock() {
        // Emulated time running at a quarter of the wall's.
        let t0 = Instant::now();
        let slow = move || Some((t0.elapsed().as_nanos() / 4) as u64);
        let start = Instant::now();
        hold(&slow, Duration::from_millis(100), Duration::from_secs(5)).await;
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(400) && took < Duration::from_millis(2000), "{took:?}");
        // No clock: the wall's.
        let start = Instant::now();
        hold(&|| None, Duration::from_millis(100), Duration::from_secs(5)).await;
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(100) && took < Duration::from_millis(1000), "{took:?}");
        // A clock that stopped: given up on after the slack.
        let start = Instant::now();
        hold(&|| Some(7), Duration::from_millis(50), Duration::from_millis(200)).await;
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(250) && took < Duration::from_millis(1500), "{took:?}");
    }

    #[tokio::test]
    async fn a_hold_across_a_chip_reset_keeps_counting() {
        // The firmware's clock restarts at 0 when the chip resets 20 ms in
        // (kodeOS going home reboots): the hold goes on counting from there.
        let t0 = Instant::now();
        let resetting = move || {
            let ns = t0.elapsed().as_nanos() as u64;
            Some(if ns < 20_000_000 { 1_000_000_000 + ns } else { ns - 20_000_000 })
        };
        let start = Instant::now();
        hold(&resetting, Duration::from_millis(100), Duration::from_secs(5)).await;
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(100) && took < Duration::from_millis(1000), "{took:?}");
    }

    #[test]
    fn a_scenario_s_values_are_of_their_kinds() {
        assert!(bad(json!({"timeout_s": "long"})).contains("\"timeout_s\" is a number of seconds"));
        assert!(bad(json!({"steps": {"wait": 1}})).contains("\"steps\" is a list"));
        assert!(bad(json!({"forbid": "x"})).contains("\"forbid\" is a list"));
    }
}

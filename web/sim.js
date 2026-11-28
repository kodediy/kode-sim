// The Kode Dot in the browser: the bridge's /ws, drawn and played.
//
// Frames are the wire's own ('K' 'D', type, u32 length, payload), one or more
// per binary message; JSON text messages carry the console, the power state
// and the battery.

const T = {
  HELLO: 0x01, FB_RECT: 0x10, PANEL: 0x11, LED: 0x20, HAPTIC: 0x21,
  PCM_OUT: 0x30, MIC_START: 0x31, MIC_STOP: 0x32, POWER_OFF: 0x40,
  TOUCH: 0x80, BUTTONS: 0x81, PCM_IN: 0x82, BATTERY: 0x83, MOTION: 0x8D,
};
const BTN = { top: 1, bot: 2, left: 4, right: 8, up: 16, down: 32 };
const W = 410, H = 502;

const $ = (id) => document.getElementById(id);
const device = $("device"), canvas = $("screen"), ctx2d = canvas.getContext("2d");
const image = ctx2d.createImageData(W, H);
let ws = null;
let state = "off";

// ---- Wire --------------------------------------------------------------

function frame(type, payload = new Uint8Array(0)) {
  const out = new Uint8Array(7 + payload.length);
  out[0] = 0x4b; out[1] = 0x44; out[2] = type;
  new DataView(out.buffer).setUint32(3, payload.length, true);
  out.set(payload, 7);
  return out;
}

// Only on an open socket: one still CONNECTING (a reconnect) throws
// InvalidStateError, and what is pressed then is not for a firmware to see.
function send(type, payload) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(frame(type, payload));
}

function sendJson(j) {
  if (!ws || ws.readyState !== WebSocket.OPEN) return false;
  ws.send(JSON.stringify(j));
  return true;
}

function* frames(buf) {
  const dv = new DataView(buf);
  let i = 0;
  while (i + 7 <= buf.byteLength) {
    if (dv.getUint8(i) !== 0x4b || dv.getUint8(i + 1) !== 0x44) { i++; continue; }
    const type = dv.getUint8(i + 2), len = dv.getUint32(i + 3, true);
    if (i + 7 + len > buf.byteLength) return;
    yield [type, buf, i + 7, len];
    i += 7 + len;
  }
}

// Next to the page, whatever site and path it is served from: the Mac's
// localhost, or wherever a proxy in front of it puts it.
const WS_URL = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}` +
  `${location.pathname.replace(/[^/]*$/, "")}ws`;
let retryMs = 1000;

// The Mac's microphone is on unless it was turned off here (remembered).
function micWanted() {
  try { return localStorage.getItem("kode-emu.mic") !== "off"; } catch (_) { return true; }
}

function connect() {
  ws = new WebSocket(WS_URL);
  ws.binaryType = "arraybuffer";
  // The sound starts by itself where the browser lets it (kode-emu's own
  // window does); elsewhere the first click on the device starts it.
  ws.onopen = () => {
    audioUnlock().catch(() => {});
    if (micWanted()) sendJson({ mic: true });
  };
  ws.onmessage = (m) => {
    if (typeof m.data === "string") onJson(JSON.parse(m.data));
    else for (const f of frames(m.data)) onFrame(...f);
  };
  ws.onclose = () => {
    micStop();
    if (state !== "busy") setState("off", "No connection to kode-emu; trying again…", true);
    setTimeout(connect, retryMs);
    retryMs = 1000;
  };
}

function onFrame(type, buf, off, len) {
  switch (type) {
    case T.FB_RECT: return drawRect(buf, off, len);
    case T.PANEL: return panel(new Uint8Array(buf, off, 2));
    case T.LED: return led(new Uint8Array(buf, off, 3));
    case T.HAPTIC: { const dv = new DataView(buf, off, 3); return haptic(dv.getUint8(0), dv.getUint16(1, true)); }
    case T.PCM_OUT: return pcmOut(buf, off, len);
    case T.MIC_START: { const dv = new DataView(buf, off, 5); return micStart(dv.getUint32(0, true)); }
    case T.MIC_STOP: return micStop();
  }
}

function onJson(j) {
  if ("console" in j) log(j.console);
  if ("state" in j) {
    if (j.state === "busy") retryMs = 15000;   // every simulator is taken: ask again in a while
    setState(j.state, j.detail);
  }
  if ("sd" in j) $("sdin").checked = !!j.sd;
  if ("usb" in j) usbPanel(j.usb);
  if ("battery" in j) { $("pct").value = j.battery.percent; $("pctv").textContent = `${j.battery.percent} %`; $("usb").checked = j.battery.usb; }
}


// ---- The computer on the USB cable ---------------------------------------
// usbhost.rs enumerates, mounts and types; this only shows it and asks.

const USB_STAGE = {
  no_cable: "no cable", waiting: "cable plugged in: nothing answers", enumerating: "enumerating…",
  mounted: "mounted", keyboard: "keyboard", ejected: "ejected: the cable can come out now", failed: "failed",
};
let usbCable = false;

function usbCmd(c) { sendJson({ usb: c }); }

function usbBytes(n) { return n >= 1 << 20 ? `${(n / (1 << 20)).toFixed(1)} MB` : n >= 1024 ? `${(n / 1024).toFixed(1)} KB` : `${n} B`; }

function usbPanel(u) {
  usbCable = u.cable;
  const st = $("usbst");
  st.className = `st ${u.stage}`;
  st.textContent = USB_STAGE[u.stage] + (u.error ? `: ${u.error}` : "");
  $("usbplug").textContent = u.cable ? "Unplug cable" : "Plug in cable";
  $("usbeject").hidden = u.stage !== "mounted" || !u.disk;
  const dev = $("usbdev");
  dev.hidden = !u.device;
  if (u.device) dev.textContent = `${u.device.product || "device"} · ${u.device.class} · ${u.device.vid}:${u.device.pid}`;
  const disk = $("usbdisk");
  disk.hidden = !(u.disk && u.stage === "mounted");
  if (!disk.hidden) {
    $("usbvol").textContent = `${u.disk.label || "no name"} · ${usbBytes(u.disk.free)} free of ${usbBytes(u.disk.total)}${u.disk.write_protected ? " · write-protected" : ""}`;
    const ul = $("usbfiles");
    ul.replaceChildren(...u.disk.files.map((f) => {
      const li = document.createElement("li");
      li.title = f.dir ? "" : "Read the file and check its CRC";
      const name = document.createElement("span"), sz = document.createElement("span");
      name.textContent = (f.dir ? "▸ " : "") + f.name; sz.className = "sz"; sz.textContent = f.dir ? "" : usbBytes(f.size);
      li.append(name, sz);
      if (!f.dir) li.onclick = () => usbCmd({ cmd: "get", name: f.name });
      return li;
    }));
    $("usblast").textContent = u.last || "";
  }
  $("usbkbd").hidden = u.stage !== "keyboard";
  if (u.stage === "keyboard") {
    $("usbtyped").textContent = u.typed;
    if ($("usblayout").value !== u.layout) $("usblayout").value = u.layout;
  }
  const log = $("usblog"), atEnd = log.scrollTop + log.clientHeight >= log.scrollHeight - 4;
  log.textContent = u.log.join("\n");
  if (atEnd) log.scrollTop = log.scrollHeight;
}

$("usbplug").onclick = () => {
  if (!usbCable && !$("usb").checked) { $("usb").checked = true; battery(); }   // the cable powers the board
  usbCmd({ cmd: usbCable ? "unplug" : "plug" });
};
$("usbeject").onclick = () => usbCmd({ cmd: "eject" });
$("usbclear").onclick = () => usbCmd({ cmd: "clear_typed" });
$("usblayout").onchange = () => usbCmd({ cmd: "layout", layout: $("usblayout").value });
$("usbrand").onclick = () => usbCmd({ cmd: "put_random", name: `aleatorio-${Date.now() % 100000}.bin`, size: 1 << 20 });
$("usbput").onchange = async () => {
  const f = $("usbput").files[0];
  $("usbput").value = "";
  if (!f) return;
  if (f.size > 700 * 1024) { $("usblast").textContent = 'that file is over 700 KB: use "Copy 1 MB" to measure the speed'; return; }
  const bytes = new Uint8Array(await f.arrayBuffer());
  let s = "";
  for (let i = 0; i < bytes.length; i += 8192) s += String.fromCharCode(...bytes.subarray(i, i + 8192));
  usbCmd({ cmd: "put", name: f.name, data: btoa(s) });
};

// ---- The glass -----------------------------------------------------------

const R5 = new Uint8Array(32).map((_, i) => (i * 527 + 23) >> 6);
const G6 = new Uint8Array(64).map((_, i) => (i * 259 + 33) >> 6);

function drawRect(buf, off, len) {
  const dv = new DataView(buf, off, 8);
  const x = dv.getUint16(0, true), y = dv.getUint16(2, true), w = dv.getUint16(4, true), h = dv.getUint16(6, true);
  if (!w || !h || x + w > W || y + h > H || len < 8 + w * h * 2) return;
  const px = new Uint16Array(buf.slice(off + 8, off + 8 + w * h * 2));
  const d = image.data;
  for (let r = 0; r < h; r++) {
    let o = ((y + r) * W + x) * 4, s = r * w;
    for (let c = 0; c < w; c++, s++, o += 4) {
      const v = px[s];
      d[o] = R5[v >> 11]; d[o + 1] = G6[(v >> 5) & 63]; d[o + 2] = R5[v & 31]; d[o + 3] = 255;
    }
  }
  ctx2d.putImageData(image, 0, 0, x, y, w, h);
}

function panel([on, brightness]) {
  // The AMOLED dims by brightness; at 0 % or off it is black.
  $("dim").style.opacity = on ? String(1 - Math.pow(brightness / 100, 0.6)) : "1";
}

function led([r, g, b]) {
  const el = $("led"), m = Math.max(r, g, b);
  if (!m) { el.style.opacity = 0; return; }
  const k = 255 / m, c = `rgb(${r * k | 0},${g * k | 0},${b * k | 0})`;
  el.style.background = `radial-gradient(circle, #fff 0%, ${c} 22%, ${c} 45%, transparent 72%)`;
  el.style.boxShadow = `0 0 28px 10px ${c}`;
  el.style.opacity = String(0.45 + 0.55 * (m / 255));
}

let shakeTimer = 0;
function haptic(strength, ms) {
  clearTimeout(shakeTimer);
  if (!strength) { device.classList.remove("shake"); return; }
  device.style.setProperty("--amp", `${(1 + strength / 12).toFixed(1)}px`);
  device.classList.add("shake");
  shakeTimer = setTimeout(() => device.classList.remove("shake"), Math.max(ms, 60));
}

function setState(s, detail = "", local = false) {
  state = s;
  const el = $("state");
  el.className = s;
  el.textContent = { running: "on", off: "off", crashed: "crashed", busy: "waiting" }[s] || s;
  $("power").textContent = s === "running" ? "On" : "Power on";
  $("power").disabled = s === "running";
  $("reset").disabled = s !== "running";
  const b = $("banner");
  b.innerHTML = "";
  if (s === "running") { b.classList.remove("show"); return; }
  micStop();   // a firmware that is off or crashed records nothing
  if (s === "off") { image.data.fill(0); ctx2d.putImageData(image, 0, 0); led([0, 0, 0]); }
  const p = document.createElement("div");
  p.textContent = local ? detail : s === "off"
    ? "Off. Press the bottom button to power it on."
    : s === "busy"
    ? "Every simulator is in use. This page tries again by itself every 15 s."
    : "The firmware crashed.";
  b.append(p);
  if (s === "crashed" && detail) { const pre = document.createElement("pre"); pre.textContent = detail; b.append(pre); }
  if (!local && s !== "busy") { const btn = document.createElement("button"); btn.textContent = s === "off" ? "Power on" : "Restart"; btn.onclick = powerOn; b.append(btn); }
  b.classList.add("show");
}

function powerOn() { sendJson({ power: "on" }); }
$("power").onclick = powerOn;
// An app built for the simulator (kode-emu build prints its .bin), into this
// Kode Dot's kodeOS, which then boots it.
$("app").onchange = async () => {
  const file = $("app").files[0];
  $("app").value = "";
  if (!file) return;
  log(`[kode-emu] uploading ${file.name} (${Math.round(file.size / 1024)} KB)…`);
  try {
    const r = await fetch("api/install", { method: "POST", body: file,
                                           headers: { "Content-Type": "application/octet-stream" } });
    const j = await r.json();
    log(j.ok ? `[kode-emu] ${file.name} installed; kodeOS starts it`
             : `[kode-emu] could not install ${file.name}: ${j.error}`);
  } catch (err) {
    log(`[kode-emu] could not upload ${file.name}: ${err}`);
  }
};

// The chip's reset, as the board's reset circuit gives it (BOT + PAD_LEFT, ~1 s).
$("reset").onclick = () => sendJson({ power: "reset" });

// ---- The finger ----------------------------------------------------------

const glass = $("glass");
let fingerDown = false, lastMove = 0;

function touch(e, pressed) {
  const r = glass.getBoundingClientRect();
  const x = Math.max(0, Math.min(W - 1, Math.round((e.clientX - r.left) / r.width * W)));
  const y = Math.max(0, Math.min(H - 1, Math.round((e.clientY - r.top) / r.height * H)));
  const p = new Uint8Array(5), dv = new DataView(p.buffer);
  p[0] = pressed ? 1 : 0; dv.setUint16(1, x, true); dv.setUint16(3, y, true);
  send(T.TOUCH, p);
}

// A press lasts at least this long, as one does on a board: the firmware
// samples and debounces, and a 0 ms click would reach it as a release alone.
const MIN_PRESS_MS = 80;
let fingerAt = 0;
// The lift put off until the touch has lasted MIN_PRESS_MS; a finger back on
// the glass before it is due takes it back, as with the buttons.
let liftLater = null;

glass.addEventListener("pointerdown", (e) => {
  clearTimeout(liftLater); liftLater = null;
  audioUnlock(); fingerDown = true; fingerAt = performance.now();
  glass.setPointerCapture(e.pointerId); touch(e, true);
});
glass.addEventListener("pointermove", (e) => {
  if (!fingerDown || e.timeStamp - lastMove < 33) return;
  lastMove = e.timeStamp; touch(e, true);
});
for (const ev of ["pointerup", "pointercancel"]) {
  glass.addEventListener(ev, (e) => {
    if (!fingerDown) return;
    fingerDown = false;
    const at = { clientX: e.clientX, clientY: e.clientY };
    liftLater = setTimeout(() => { liftLater = null; touch(at, false); },
                           Math.max(0, MIN_PRESS_MS - (performance.now() - fingerAt)));
  });
}

// ---- Buttons -------------------------------------------------------------

let mask = 0;
const pressedAt = {};
// One release put off per button, until its press has lasted MIN_PRESS_MS.
// A press before it is due takes it back: a tap and then a hold within
// 80 ms reach the firmware as one press, let go when the hold is.
const releaseLater = {};
function button(name, down) {
  clearTimeout(releaseLater[name]);
  delete releaseLater[name];
  if (down) {
    pressedAt[name] = performance.now();
  } else {
    const left = MIN_PRESS_MS - (performance.now() - (pressedAt[name] ?? -Infinity));
    if (left > 0) {
      releaseLater[name] = setTimeout(() => { delete releaseLater[name]; setButton(name, false); }, left);
      return;
    }
  }
  setButton(name, down);
}

function setButton(name, down) {
  const bit = BTN[name], next = down ? mask | bit : mask & ~bit;
  document.querySelector(`[data-btn="${name}"]`).classList.toggle("down", down);
  if (next === mask) return;
  mask = next;
  send(T.BUTTONS, new Uint8Array([mask]));
}

for (const el of document.querySelectorAll("[data-btn]")) {
  const name = el.dataset.btn;
  el.addEventListener("pointerdown", (e) => { audioUnlock(); el.setPointerCapture(e.pointerId); button(name, true); });
  el.addEventListener("pointerup", () => button(name, false));
  el.addEventListener("pointercancel", () => button(name, false));
}

const KEYS = { t: "top", T: "top", b: "bot", B: "bot", ArrowUp: "up", ArrowDown: "down", ArrowLeft: "left", ArrowRight: "right" };
for (const [ev, down] of [["keydown", true], ["keyup", false]]) {
  window.addEventListener(ev, (e) => {
    if (e.target === $("cmd") || e.repeat || !(e.key in KEYS)) return;
    e.preventDefault(); audioUnlock(); button(KEYS[e.key], down);
  });
}

// ---- Battery ---------------------------------------------------------------

function battery() {
  $("pctv").textContent = `${$("pct").value} %`;
  send(T.BATTERY, new Uint8Array([Number($("pct").value), $("usb").checked ? 1 : 0]));
}
$("pct").oninput = battery;
$("usb").onchange = battery;
// The microSD's slot: taking the card out is what kodeOS sees as a card pulled.
$("sdin").onchange = () => sendJson({ sd: $("sdin").checked });
// The Mac's microphone: kode-emu turns it on for every page with MIC_START,
// at the rate the firmware's receiver runs at. A click is also the gesture
// the sound needs.
$("micin").onchange = () => {
  if ($("micin").checked) audioUnlock().catch(() => {});
  try { localStorage.setItem("kode-emu.mic", $("micin").checked ? "on" : "off"); } catch (_) { /* not kept */ }
  sendJson({ mic: $("micin").checked });
};

// ---- Motion: the IMU (QMI8658A) and the magnetometer (QMC5883P) ---------------
//
// The board is posed by three angles and the page works out what the chips
// would measure in the board's own axes (x right, y up the screen, z out of
// the screen), which is what the bridge's MOTION frame carries: ten i32, little
// endian: accelerometer mg x3, gyroscope mdps x3, magnetometer mG x3, and the
// temperature in tenths of a degree. Flat and face up it reads +1 g on z; the
// heading is clockwise from north, the compass's own convention; the Earth's
// field is the one at Madrid (about 25 uT horizontal, 38 uT down).
const EARTH_H = 250, EARTH_V = 380;    // mG
const MAGNET_FAR = 160;                // slider end = no magnet

// World (east, north, up) to the board: the heading turns about up, then
// the pitch about the board's x and the roll about its y.
function bodyOf(w, pitch, roll, heading) {
  const d = Math.PI / 180, ps = heading * d, th = pitch * d, ph = roll * d;
  const cp = Math.cos(ps), sp = Math.sin(ps), ct = Math.cos(th), st = Math.sin(th), cr = Math.cos(ph), sr = Math.sin(ph);
  const mul = (a, b) => a.map((r) => [0, 1, 2].map((j) => r[0] * b[0][j] + r[1] * b[1][j] + r[2] * b[2][j]));
  // R takes board coordinates to the world's; its columns are the board's axes there
  const R = mul(mul([[cp, sp, 0], [-sp, cp, 0], [0, 0, 1]], [[1, 0, 0], [0, ct, -st], [0, st, ct]]),
                [[cr, 0, sr], [0, 1, 0], [-sr, 0, cr]]);
  return [0, 1, 2].map((j) => R[0][j] * w[0] + R[1][j] * w[1] + R[2][j] * w[2]);     // R^T w
}

function motion(p, t = 0) {
  const acc = bodyOf([0, 0, 1000], p.pitch, p.roll, p.heading);
  const mag = bodyOf([0, EARTH_H, -EARTH_V], p.pitch, p.roll, p.heading);
  const gyro = [0, 0, 0];
  if (p.magnet < MAGNET_FAR) {          // a dipole's field falls with the cube of the distance; along z, through the back
    const b = 60000 * Math.pow(10 / p.magnet, 3);
    mag[2] -= b;
  }
  if (t > 0) {                           // a shake: about 6 Hz, up to 2 g and 400 deg/s, dying away
    const k = Math.max(0, 1 - t / SHAKE_MS) ** 0.5, w = 2 * Math.PI * 6 * t / 1000;
    for (let i = 0; i < 3; i++) {
      acc[i] += 2000 * k * Math.sin(w + i * 2.1);
      gyro[i] = 400000 * k * Math.cos(w * 1.3 + i * 1.7);
    }
  }
  return [...acc, ...gyro, ...mag, 250].map(Math.round);
}

const SHAKE_MS = 1200;
let shakeAt = -1, motionTimer = null;
const pose = () => ({ pitch: Number($("mpitch").value), roll: Number($("mroll").value),
                      heading: Number($("mhead").value), magnet: Number($("mmag").value) });

function motionSend() {
  const t = shakeAt < 0 ? 0 : Math.max(1, performance.now() - shakeAt);
  if (t > SHAKE_MS) shakeAt = -1;
  const m = motion(pose(), shakeAt < 0 ? 0 : t);
  const out = new Uint8Array(40);
  const dv = new DataView(out.buffer);
  m.forEach((v, i) => dv.setInt32(i * 4, Math.max(-2147483647, Math.min(2147483647, v)), true));
  send(T.MOTION, out);
  const p = pose();
  $("mpitchv").textContent = `${p.pitch}°`; $("mrollv").textContent = `${p.roll}°`;
  $("mheadv").textContent = `${p.heading}° ${["N", "NE", "E", "SE", "S", "SW", "W", "NW"][Math.round(p.heading / 45) % 8]}`;
  $("mmagv").textContent = p.magnet >= MAGNET_FAR ? "no magnet" : `${p.magnet} mm`;
  $("mread").textContent = `a ${m.slice(0, 3).map((v) => (v / 1000).toFixed(2)).join(" ")} g · m ${m.slice(6, 9).map((v) => (v / 10).toFixed(0)).join(" ")} µT`;
  if (shakeAt >= 0 && !motionTimer) motionTimer = setTimeout(() => { motionTimer = null; motionSend(); }, 33);
}
for (const id of ["mpitch", "mroll", "mhead", "mmag"]) $(id).oninput = motionSend;
$("mshake").onclick = () => { shakeAt = performance.now(); motionSend(); };
$("mflat").onclick = () => {
  for (const [id, v] of [["mpitch", 0], ["mroll", 0], ["mhead", 0], ["mmag", MAGNET_FAR]]) $(id).value = v;
  motionSend();
};

// ---- Console ----------------------------------------------------------------

const logEl = $("log"), LOG_LINES = 2000;
// Lines wait for the next frame and go in together: one layout for the
// ~500 a page is sent on connecting, not one each.
let logQueue = [], logFlushing = false;
function log(line) {
  logQueue.push(line);
  if (logQueue.length > 2 * LOG_LINES) logQueue = logQueue.slice(-LOG_LINES);   // a hidden tab draws no frames
  if (!logFlushing) { logFlushing = true; requestAnimationFrame(logFlush); }
}

function logFlush() {
  logFlushing = false;
  const lines = logQueue.slice(-LOG_LINES);
  logQueue = [];
  const atEnd = logEl.scrollTop + logEl.clientHeight >= logEl.scrollHeight - 4;
  logEl.append(...lines.map((line) => {
    const div = document.createElement("div");
    div.textContent = line.replace(/\x1b\[[0-9;]*[A-Za-z]/g, "");
    if (/^!E|^E \(|Guru|abort/.test(line)) div.className = "e";
    else if (/^!W|^W \(/.test(line)) div.className = "w";
    return div;
  }));
  for (let n = logEl.childElementCount - LOG_LINES; n > 0; n--) logEl.firstChild.remove();
  if (atEnd) logEl.scrollTop = logEl.scrollHeight;
}
$("cmd").addEventListener("keydown", (e) => {
  if (e.key !== "Enter") return;
  if (sendJson({ console_in: $("cmd").value })) $("cmd").value = "";   // kept to send again if not connected
});

// ---- Speaker and microphone -------------------------------------------------

let ac = null, speaker = null, micNode = null, micStream = null, micRate = 48000;
// The context and its worklet, made once: an unlock while the first is
// still loading the worklet waits for it, as the microphone needs it there.
let audioReady = null;

async function audioUnlock() {
  audioReady ||= (async () => {
    ac = new AudioContext({ sampleRate: 48000 });
    await ac.audioWorklet.addModule("audio-worklet.js");
    speaker = new AudioWorkletNode(ac, "kode-speaker", { outputChannelCount: [2] });
    speaker.connect(ac.destination);
  })().catch((e) => { audioReady = null; ac = null; speaker = null; throw e; });   // the next gesture tries again
  await audioReady;
  if (ac.state !== "running") await ac.resume();
  $("sound").textContent = "Sound on";
  $("sound").style.color = "var(--muted)";
}

function pcmOut(buf, off, len) {
  if (!speaker || len < 5) return;
  const dv = new DataView(buf, off, 5), rate = dv.getUint32(0, true), ch = dv.getUint8(4);
  const s = new Int16Array(buf.slice(off + 5, off + len));
  speaker.port.postMessage({ rate, ch, s }, [s.buffer]);
}

// Bumped by every micStop: a micStart whose await outlived a MIC_STOP, a
// second MIC_START, the socket closing or the firmware going off is stale.
let micGen = 0;

async function micStart(rate) {
  micStop();   // never two streams: a second MIC_START finds the last one gone
  $("micin").checked = true;
  const gen = micGen;
  micRate = rate;
  try {
    await audioUnlock();
    if (gen !== micGen) return;
    // Echo cancellation on: the Kode Dot hearing its own speaker is kode-emu's
    // to model (through the case, on its clock); the Mac's speakers coming back
    // through the Mac's microphone would be a second, louder copy of it.
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: { echoCancellation: true, noiseSuppression: false, autoGainControl: false } });
    if (gen !== micGen) { stream.getTracks().forEach((t) => t.stop()); return; }
    micStream = stream;
    const src = ac.createMediaStreamSource(micStream);
    // No outputs: a sink, which every engine runs without it reaching the speakers.
    micNode = new AudioWorkletNode(ac, "kode-mic", { numberOfOutputs: 0, processorOptions: { rate: micRate } });
    micNode.port.onmessage = (m) => send(T.PCM_IN, new Uint8Array(m.data));
    src.connect(micNode);
    log("[kode-emu] the computer's microphone → the Kode Dot's microphone");
  } catch (err) {
    if (gen !== micGen) return;
    log(`[kode-emu] no microphone (${err.name}); the Kode Dot hears only its own speaker and its noise floor`);
    micStop();
    sendJson({ mic: false });
  }
}

function micStop() {
  micGen++;
  $("micin").checked = false;
  if (micNode) { micNode.disconnect(); micNode = null; }
  if (micStream) { micStream.getTracks().forEach((t) => t.stop()); micStream = null; }
}

// ---- Fit the device to the window -------------------------------------------

function fit() {
  const st = $("stage"), k = Math.min(st.clientWidth / 602, st.clientHeight / 979, 1.2);
  device.style.transform = `scale(${k})`;
  device.style.margin = `${(979 * k - 979) / 2}px ${(602 * k - 602) / 2}px`;
}
window.addEventListener("resize", fit);
new ResizeObserver(fit).observe($("stage"));   // the stage changes when the side panel does (?embed=1)
fit();
connect();

// How fast the Kode Dot runs: its emulated clock (api/stats) read every 2 s
// against this page's. 1.00 is a board's own pace; below it, the emulator
// cannot keep up on this computer.
let paceAt = null;   // the last reading: [the page's clock, the chip's], in ms
async function pace() {
  setTimeout(pace, 2000);
  if (!ws || ws.readyState !== WebSocket.OPEN) { paceAt = null; return; }
  try {
    const now = performance.now(), j = await (await fetch("api/stats")).json();
    // A clock that went back is a reset; one that stands still is a Kode Dot that is off.
    if (paceAt && j.sim_ms > paceAt[1]) $("speed").textContent = `${((j.sim_ms - paceAt[1]) / (now - paceAt[0])).toFixed(2)}× real time`;
    else if (paceAt) $("speed").textContent = "…";
    paceAt = [now, j.sim_ms];
  } catch (_) { paceAt = null; }
}
setTimeout(pace, 2000);

// "Settings": the controls and the console, over the Kode Dot, which is all
// the window shows otherwise.
$("settings").onclick = () => { document.documentElement.classList.toggle("panel"); };

// "More" (only shown in ?embed=1): the rest of the controls and the console.
$("more").onclick = () => { const on = document.documentElement.classList.toggle("more"); $("more").textContent = on ? "Less" : "More"; fit(); };

// ---- Inside another page (?embed=1) -----------------------------------------
// The host page cannot see a click or a key that lands in the frame: tell it
// someone is using the Kode Dot (at most every 10 s), so it can stop a
// simulator nobody touches. The message carries nothing else.
if (window.parent !== window && document.documentElement.classList.contains("embed")) {
  let told = -Infinity;           // the first use is told at once, however soon after loading
  const activity = () => {
    const now = performance.now();
    if (now - told < 10000) return;
    told = now;
    window.parent.postMessage({ kodeEmu: "activity" }, "*");
  };
  for (const ev of ["pointerdown", "keydown"]) window.addEventListener(ev, activity, true);
}

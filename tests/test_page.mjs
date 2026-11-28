// web/sim.js in node, on a mock DOM, WebSocket, clock and audio: what the
// page sends, and when. node --test tests/test_page.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const SRC = readFileSync(new URL("../web/sim.js", import.meta.url), "utf8");
const T = { BUTTONS: 0x81, PCM_IN: 0x82 };

function element(id) {
  const listeners = {}, children = [];
  let scrollHeightReads = 0;
  return {
    id, listeners, children, style: { setProperty() {} }, dataset: {},
    classList: { add() {}, remove() {}, toggle() {} },
    textContent: "", value: "", checked: false, disabled: false, innerHTML: "", className: "",
    scrollTop: 0, clientHeight: 100,
    get scrollHeight() { scrollHeightReads++; return 100 + children.length; },
    get scrollHeightReads() { return scrollHeightReads; },
    addEventListener(ev, f) { (listeners[ev] ||= []).push(f); },
    setPointerCapture() {},
    getBoundingClientRect: () => ({ left: 0, top: 0, width: 410, height: 502 }),
    getContext: () => ({ createImageData: (w, h) => ({ data: new Uint8ClampedArray(w * h * 4) }), putImageData() {} }),
    append(...c) { children.push(...c); },
    replaceChildren(...c) { children.splice(0, children.length, ...c); },
    hidden: false,
    get childElementCount() { return children.length; },
    get firstChild() { return children.length ? { remove: () => children.shift() } : null; },
  };
}

// A page loaded on mocks; `tick(ms)` moves its clock and runs what is due.
function page(opts = {}) {
  const els = {};
  let now = 1000, seq = 0;          // a page has been up a while
  const timers = new Map();
  const frames = [];
  const sockets = [];
  const media = [];                  // getUserMedia calls: {resolve, reject}
  const micNodes = [];
  const nodes = [];                  // every AudioWorkletNode, by name
  const winListeners = {}, posted = [];   // the window's listeners; what it told its parent
  const modules = [];                // addModule calls left pending, when slowModule
  let slowModule = false;
  // The page's root classes: "embed" when ?embed=1 put it there.
  const roots = new Set(opts.embed ? ["embed"] : []);
  const rootClasses = {
    add: (c) => roots.add(c), remove: (c) => roots.delete(c), contains: (c) => roots.has(c),
    toggle: (c, on) => { const v = on === undefined ? !roots.has(c) : on; v ? roots.add(c) : roots.delete(c); return v; },
  };
  const store = new Map(Object.entries(opts.stored || {}));   // localStorage

  class WebSocket {
    static CONNECTING = 0; static OPEN = 1; static CLOSING = 2; static CLOSED = 3;
    constructor(url) { this.url = url; this.readyState = 0; this.sent = []; sockets.push(this); }
    send(d) {
      if (this.readyState !== 1) { const e = new Error("Still in CONNECTING state."); e.name = "InvalidStateError"; throw e; }
      this.sent.push(d);
    }
  }
  class AudioContext {
    constructor() {
      this.state = "running"; this.destination = {};
      this.audioWorklet = { addModule: () => (slowModule ? new Promise((r) => modules.push(r)) : Promise.resolve()) };
    }
    async resume() { this.state = "running"; }
    createMediaStreamSource() { return { connect() {} }; }
  }
  class AudioWorkletNode {
    constructor(ac, name) { this.name = name; this.port = { postMessage() {}, onmessage: null }; this.live = true;
                            nodes.push(name);
                            if (name === "kode-mic") micNodes.push(this); }
    connect() {}
    disconnect() { this.live = false; }
  }
  function stream() {
    const track = { stopped: false, stop() { this.stopped = true; } };
    return { track, getTracks: () => [track] };
  }
  const sandbox = {
    console,
    document: {
      getElementById: (id) => (els[id] ||= element(id)),
      createElement: () => element("created"),
      querySelector: (sel) => (els[sel] ||= element(sel)),
      querySelectorAll: () => [],
      documentElement: { classList: rootClasses },
    },
    window: {
      addEventListener(ev, f) { (winListeners[ev] ||= []).push(f); },
      parent: { postMessage(m) { posted.push(m); } },
    },
    location: { protocol: "http:", host: "127.0.0.1:8765", pathname: "/" },
    performance: { now: () => now },
    setTimeout: (f, ms = 0) => { const id = ++seq; timers.set(id, { at: now + Math.max(0, ms), f }); return id; },
    clearTimeout: (id) => timers.delete(id),
    requestAnimationFrame: (f) => { frames.push(f); return frames.length; },
    navigator: { mediaDevices: { getUserMedia: () => new Promise((resolve, reject) => media.push({ resolve, reject })) } },
    WebSocket, AudioContext, AudioWorkletNode,
    fetch: async (url, init) => (opts.fetch || (() => ({ ok: false, status: 404, json: async () => ({}) })))(url, init),
    localStorage: { getItem: (k) => (store.has(k) ? store.get(k) : null), setItem: (k, v) => store.set(k, String(v)) },
    ResizeObserver: class { observe() {} },
  };
  vm.createContext(sandbox);
  vm.runInContext(SRC, sandbox);

  const p = {
    sb: sandbox, els, sockets, media, micNodes, stream, nodes, modules, posted, roots, store,
    fire(ev) { for (const f of winListeners[ev] || []) f({}); },
    slowModules() { slowModule = true; },
    get now() { return now; },
    get ws() { return sockets[sockets.length - 1]; },
    open() { p.ws.readyState = 1; return p.ws; },
    tick(ms) {
      const end = now + ms;
      for (;;) {
        let next = null;
        for (const [id, t] of timers) if (t.at <= end && (!next || t.at < next[1].at)) next = [id, t];
        if (!next) break;
        timers.delete(next[0]);
        now = next[1].at;
        next[1].f();
      }
      now = end;
    },
    frame() { for (const f of frames.splice(0)) f(now); },
    buttons() { return p.ws.sent.filter((d) => typeof d !== "string" && d[2] === T.BUTTONS).map((d) => d[7]); },
    pcmIn() { return p.ws.sent.filter((d) => typeof d !== "string" && d[2] === T.PCM_IN).length; },
  };
  return p;
}

const settle = () => new Promise((r) => setImmediate(r));

// ---- the microphone ------------------------------------------------------------

test("a MIC_STOP while the microphone is being asked for leaves no stream", async () => {
  const p = page(); p.open();
  p.sb.micStart(16000);
  await settle();
  assert.equal(p.media.length, 1);
  p.sb.micStop();
  const s = p.stream();
  p.media[0].resolve(s);
  await settle();
  assert.equal(s.track.stopped, true, "the stream asked for before the stop is still recording");
  assert.equal(p.micNodes.length, 0);
});

test("a second MIC_START while the first is pending leaves one stream", async () => {
  const p = page(); p.open();
  p.sb.micStart(16000);
  await settle();
  p.sb.micStart(48000);
  await settle();
  assert.equal(p.media.length, 2);
  const first = p.stream(), second = p.stream();
  p.media[0].resolve(first);
  p.media[1].resolve(second);
  await settle();
  assert.equal(first.track.stopped, true);
  assert.equal(second.track.stopped, false);
  assert.equal(p.micNodes.filter((n) => n.live).length, 1);
});

test("the firmware going off while the microphone is asked for leaves no stream", async () => {
  const p = page(); p.open();
  p.sb.micStart(16000);
  await settle();
  p.sb.setState("off");
  const s = p.stream();
  p.media[0].resolve(s);
  await settle();
  assert.equal(s.track.stopped, true);
  assert.equal(p.micNodes.length, 0);
});

test("a MIC_STOP while the sound is unlocking never asks for the microphone", async () => {
  const p = page(); p.open();
  let unlock;
  p.sb.AudioContext = class extends p.sb.AudioContext {
    constructor() { super(); this.audioWorklet = { addModule: () => new Promise((r) => { unlock = r; }) }; }
  };
  p.sb.micStart(16000);
  await settle();
  p.sb.micStop();
  unlock();
  await settle();
  assert.equal(p.media.length, 0);
});

test("a started microphone sends PCM_IN until it is stopped", async () => {
  const p = page(); p.open();
  p.sb.micStart(16000);
  await settle();
  p.media[0].resolve(p.stream());
  await settle();
  const node = p.micNodes[0];
  node.port.onmessage({ data: new ArrayBuffer(64) });
  assert.equal(p.pcmIn(), 1);
  p.sb.micStop();
  assert.equal(node.live, false);
});

test("the microphone switch asks kode-emu, which turns it on for every page", async () => {
  const p = page(); p.open();
  p.els.micin.checked = true;
  p.els.micin.onchange();
  assert.deepEqual(p.ws.sent.filter((d) => typeof d === "string").map(JSON.parse), [{ mic: true }]);
  assert.equal(p.media.length, 0, "the microphone opens on kode-emu's MIC_START, not before");
});

test("MIC_START ticks the switch and MIC_STOP unticks it", async () => {
  const p = page(); p.open();
  p.sb.micStart(48000);
  assert.equal(p.els.micin.checked, true);
  p.sb.micStop();
  assert.equal(p.els.micin.checked, false);
});

test("a microphone the Mac refuses turns the switch off for every page", async () => {
  const p = page(); p.open();
  p.sb.micStart(48000);
  await settle();
  const e = new Error("denied"); e.name = "NotAllowedError";
  p.media[0].reject(e);
  await settle();
  assert.equal(p.els.micin.checked, false);
  assert.deepEqual(p.ws.sent.filter((d) => typeof d === "string").map(JSON.parse), [{ mic: false }]);
});

// ---- inside another page (?embed=1) ------------------------------------------

test("an embedded page tells its parent of the first use at once, then at most every 10 s", () => {
  const p = page({ embed: true });           // a page loaded a second ago
  p.fire("pointerdown");
  assert.equal(JSON.stringify(p.posted), JSON.stringify([{ kodeEmu: "activity" }]), "the first press, right after loading");
  p.tick(5000);
  p.fire("keydown");
  assert.equal(p.posted.length, 1, "5 s later: not again yet");
  p.tick(6000);
  p.fire("keydown");
  assert.equal(p.posted.length, 2);
});

test("a page that is not embedded tells nobody", () => {
  const p = page();
  p.fire("pointerdown");
  assert.deepEqual(p.posted, []);
});

// ---- buttons --------------------------------------------------------------

test("a second unlock while the first loads the worklet waits for it", async () => {
  const p = page(); p.open();
  p.slowModules();
  const first = p.sb.audioUnlock();
  let secondDone = false;
  const second = p.sb.audioUnlock().then(() => { secondDone = true; });
  await settle();
  assert.equal(secondDone, false, "the second unlock came back before the worklet was there");
  p.modules.forEach((r) => r());
  await first; await second;
  assert.deepEqual(p.nodes, ["kode-speaker"]);
});

test("a tap is held at least 80 ms", () => {
  const p = page(); p.open();
  p.sb.button("top", true);
  p.tick(10); p.sb.button("top", false);
  assert.deepEqual(p.buttons(), [1]);
  p.tick(69); assert.deepEqual(p.buttons(), [1]);
  p.tick(1); assert.deepEqual(p.buttons(), [1, 0]);
});

test("a tap then a hold within 80 ms is one press, released when the hold is", () => {
  const p = page(); p.open();
  p.sb.button("top", true);
  p.tick(20); p.sb.button("top", false);     // the tap's release, put off to 80 ms
  p.tick(30); p.sb.button("top", true);      // a hold, from 50 ms
  p.tick(450);
  assert.deepEqual(p.buttons(), [1], "the hold was let go while still held");
  p.sb.button("top", false);
  assert.deepEqual(p.buttons(), [1, 0]);
});

test("a tap on the glass then a finger kept still within 80 ms stays down", () => {
  const p = page(); p.open();
  const g = p.els.glass.listeners;
  const at = { clientX: 100, clientY: 100, pointerId: 1, timeStamp: 0 };
  const fingers = () => p.ws.sent.filter((d) => typeof d !== "string" && d[2] === 0x80).map((d) => d[7]);
  g.pointerdown[0](at);
  p.tick(20); g.pointerup[0](at);          // the tap's lift, put off to 80 ms
  p.tick(30); g.pointerdown[0](at);        // the finger back, and kept still
  p.tick(450);
  assert.deepEqual(fingers(), [1, 1], "the finger was lifted while still on the glass");
  g.pointerup[0](at);
  p.tick(80);
  assert.deepEqual(fingers(), [1, 1, 0]);
});

// ---- sends while the socket reconnects --------------------------------------

test("nothing is sent, and nothing throws, while the socket is connecting", () => {
  const p = page();                           // the socket still CONNECTING
  assert.doesNotThrow(() => p.sb.powerOn());
  assert.doesNotThrow(() => p.els.reset.onclick());
  assert.doesNotThrow(() => p.els.sdin.onchange());
  p.els.cmd.value = "help";
  assert.doesNotThrow(() => p.els.cmd.listeners.keydown[0]({ key: "Enter" }));
  assert.deepEqual(p.ws.sent, []);
  assert.equal(p.els.cmd.value, "help", "the command typed was lost");
  p.open();
  p.sb.powerOn();
  p.els.cmd.listeners.keydown[0]({ key: "Enter" });
  assert.deepEqual(p.ws.sent, [JSON.stringify({ power: "on" }), JSON.stringify({ console_in: "help" })]);
  assert.equal(p.els.cmd.value, "");
});

// ---- the console ---------------------------------------------------------------

test("console lines are put on the page once a frame, the layout read once", () => {
  const p = page();
  const el = p.els.log;
  for (let i = 0; i < 500; i++) p.sb.log(i % 2 ? `E (1) bad ${i}` : `line ${i}`);
  assert.equal(el.childElementCount, 0);
  const before = el.scrollHeightReads;
  p.frame();
  assert.equal(el.childElementCount, 500);
  assert.ok(el.scrollHeightReads - before <= 2, `${el.scrollHeightReads - before} layout reads`);
  assert.equal(el.children[1].className, "e");
  assert.equal(el.children[0].textContent, "line 0");
});

test("the console keeps its last 2000 lines", () => {
  const p = page();
  for (let i = 0; i < 2500; i++) p.sb.log(`line ${i}`);
  p.frame();
  for (let i = 2500; i < 2600; i++) p.sb.log(`line ${i}`);
  p.frame();
  const el = p.els.log;
  assert.equal(el.childElementCount, 2000);
  assert.equal(el.children[0].textContent, "line 600");
  assert.equal(el.children[1999].textContent, "line 2599");
});


// ---- motion: the IMU and the magnetometer --------------------------------------------

const motionFrames = (ws) => ws.sent.filter((d) => typeof d !== "string" && d[2] === 0x8D)
  .map((d) => Array.from(new Int32Array(d.buffer.slice(d.byteOffset + 7, d.byteOffset + 47))));

// What the firmware's kcompass_heading does with a board's accelerometer and field.
function heading(m) {
  const n = Math.hypot(m[0], m[1], m[2]), up = [m[0] / n, m[1] / n, m[2] / n];
  const d = m[6] * up[0] + m[7] * up[1] + m[8] * up[2];
  const h = [m[6] - d * up[0], m[7] - d * up[1], m[8] - d * up[2]];
  const east = [h[1] * up[2] - h[2] * up[1], h[2] * up[0] - h[0] * up[2]];
  return (Math.atan2(east[1], h[1]) * 180 / Math.PI + 360) % 360;
}

test("flat, facing north, the board reads 1 g on z and the Earth's field", () => {
  const p = page(); const ws = p.open();
  p.els.mflat.onclick();
  assert.deepEqual(motionFrames(ws).at(-1), [0, 0, 1000, 0, 0, 0, 0, 250, -380, 250]);
});

test("whatever the pose, 1 g and the Earth's field keep their size, and the heading is the one set", () => {
  const p = page(); const ws = p.open();
  for (const [pitch, roll, head] of [[0, 0, 90], [30, 0, 45], [0, -40, 200], [25, 35, 310], [-60, 20, 5]]) {
    p.els.mpitch.value = pitch; p.els.mroll.value = roll; p.els.mhead.value = head; p.els.mmag.value = 160;
    p.els.mpitch.oninput();
    const m = motionFrames(ws).at(-1);
    assert.ok(Math.abs(Math.hypot(m[0], m[1], m[2]) - 1000) < 2, `|a| at ${pitch},${roll}`);
    assert.ok(Math.abs(Math.hypot(m[6], m[7], m[8]) - Math.hypot(250, 380)) < 2, `|m| at ${pitch},${roll}`);
    const dh = Math.abs(((heading(m) - head + 540) % 360) - 180);
    assert.ok(dh < 1, `heading ${heading(m).toFixed(1)} for ${head} at ${pitch},${roll}`);
  }
});

test("tilting the board moves gravity onto its own axes", () => {
  const p = page(); const ws = p.open();
  p.els.mroll.value = 90; p.els.mroll.oninput();                  // on its edge
  const a = motionFrames(ws).at(-1);
  assert.ok(Math.abs(a[2]) < 2 && Math.abs(Math.abs(a[0]) - 1000) < 2, `on its edge: ${a.slice(0, 3)}`);
  p.els.mroll.value = 0; p.els.mpitch.value = 90; p.els.mpitch.oninput();   // standing on its end
  const b = motionFrames(ws).at(-1);
  assert.ok(Math.abs(b[2]) < 2 && Math.abs(Math.abs(b[1]) - 1000) < 2, `standing: ${b.slice(0, 3)}`);
});

test("a magnet near the back adds to the field along z, and none at the slider's end", () => {
  const p = page(); const ws = p.open();
  p.els.mmag.value = 20; p.els.mmag.oninput();
  const near = motionFrames(ws).at(-1);
  assert.ok(near[8] < -380 - 4000, `field ${near[8]}`);
  p.els.mmag.value = 160; p.els.mmag.oninput();
  assert.equal(motionFrames(ws).at(-1)[8], -380);
});

test("a shake sends frames for 1.2 s, shakes the accelerometer and the gyroscope, then settles", () => {
  const p = page(); const ws = p.open();
  p.els.mshake.onclick();
  p.tick(300);
  const during = motionFrames(ws);
  assert.ok(during.length > 5, `${during.length} frames`);
  assert.ok(during.some((m) => Math.abs(m[0]) > 300 || Math.abs(m[1]) > 300) && during.some((m) => Math.abs(m[3]) > 20000));
  p.tick(1500);
  const n = motionFrames(ws).length;
  p.tick(500);
  assert.equal(motionFrames(ws).length, n, "still sending after the shake");
  assert.deepEqual(motionFrames(ws).at(-1).slice(0, 6), [0, 0, 1000, 0, 0, 0]);
});
// ---- the USB cable ---------------------------------------------------------------

test("the cable's snapshot fills the computer's panel, and its buttons send commands", () => {
  const p = page();
  const ws = p.open();
  ws.onopen?.();
  const say = (u) => ws.onmessage({ data: JSON.stringify({ usb: u }) });
  const base = { cable: true, error: "", log: ["  0.10s  cable plugged in"], read: 0, written: 0, speed: 0, last: "", layout: "es", typed: "" };
  say({ ...base, stage: "keyboard", typed: "Hola", device: { vid: "306a", pid: "1234", manufacturer: "", product: "Kode", class: "keyboard" } });
  assert.equal(p.els.usbkbd.hidden, false);
  assert.equal(p.els.usbdisk.hidden, true);
  assert.equal(p.els.usbtyped.textContent, "Hola");
  assert.equal(p.els.usbplug.textContent, "Unplug cable");
  say({ ...base, stage: "mounted", device: { vid: "306a", pid: "1234", manufacturer: "", product: "Disk", class: "mass storage" },
        disk: { label: "KODE", free: 2 << 20, total: 4 << 20, write_protected: false, files: [{ name: "a.txt", size: 19, dir: false }] } });
  assert.equal(p.els.usbdisk.hidden, false);
  assert.equal(p.els.usbkbd.hidden, true);
  assert.equal(p.els.usbeject.hidden, false);
  assert.equal(p.els.usbfiles.children.length, 1);
  assert.match(p.els.usbvol.textContent, /KODE · 2\.0 MB free of 4\.0 MB/);
  p.els.usbfiles.children[0].onclick();
  p.els.usbeject.onclick();
  const sent = ws.sent.filter((d) => typeof d === "string").map((d) => JSON.parse(d).usb).filter(Boolean);
  assert.deepEqual(sent, [{ cmd: "get", name: "a.txt" }, { cmd: "eject" }]);
  say({ ...base, cable: false, stage: "no_cable" });
  p.els.usbplug.onclick();
  assert.deepEqual(JSON.parse(ws.sent.at(-1)).usb, { cmd: "plug" });
});

// ---- The window: the Kode Dot alone, the settings behind a button ---------------

test("the settings start closed, and the button opens and closes them", () => {
  const p = page();
  assert.equal(p.roots.has("panel"), false);
  p.els.settings.onclick();
  assert.equal(p.roots.has("panel"), true);
  p.els.settings.onclick();
  assert.equal(p.roots.has("panel"), false);
});

test("on connecting, the page asks for the Mac's microphone unless it was turned off", () => {
  const p = page(); const ws = p.open(); ws.onopen();
  assert.ok(ws.sent.includes(JSON.stringify({ mic: true })), ws.sent);
  const q = page({ stored: { "kode-emu.mic": "off" } }); const w2 = q.open(); w2.onopen();
  assert.ok(!w2.sent.some((d) => typeof d === "string" && d.includes('"mic":true')), w2.sent);
});

test("turning the microphone off is remembered", () => {
  const p = page(); p.open();
  p.els.micin.checked = false; p.els.micin.onchange();
  assert.equal(p.store.get("kode-emu.mic"), "off");
  p.els.micin.checked = true; p.els.micin.onchange();
  assert.equal(p.store.get("kode-emu.mic"), "on");
});

// ---- How fast it runs ----------------------------------------------------------

test("the settings say how fast the Kode Dot runs, from two readings of its clock", async () => {
  let simMs = 1000;
  const p = page({ fetch: (url) => ({ ok: true, status: 200, json: async () => ({ sim_ms: simMs }) }) });
  p.open().onopen();
  p.tick(2000); await settle(); await settle();      // the first reading: nothing to compare with yet
  simMs += 1000;                                      // 1 s of the chip in 2 s of the wall clock
  p.tick(2000); await settle(); await settle();
  assert.equal(p.els.speed.textContent, "0.50× real time");
  simMs += 2000;
  p.tick(2000); await settle(); await settle();
  assert.equal(p.els.speed.textContent, "1.00× real time");
});

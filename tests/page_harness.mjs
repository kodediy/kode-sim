// web/sim.js on a DOM, WebSocket, clock and audio controlled by tests.
import { readFileSync } from "node:fs";
import vm from "node:vm";

const SRC = readFileSync(new URL("../web/sim.js", import.meta.url), "utf8");
const T = { BUTTONS: 0x81, PCM_IN: 0x82 };

function element(id) {
  const listeners = {}, children = [];
  const attributes = new Map(), classes = new Set();
  let scrollHeightReads = 0, text = "", draws = 0;
  return {
    id, listeners, children, style: { setProperty() {} }, dataset: {},
    classList: {
      add: (...names) => names.forEach((name) => classes.add(name)),
      remove: (...names) => names.forEach((name) => classes.delete(name)),
      contains: (name) => classes.has(name),
      toggle(name, force) { const on = force ?? !classes.has(name); on ? classes.add(name) : classes.delete(name); return on; },
    },
    get className() { return [...classes].join(" "); },
    set className(value) { classes.clear(); value.split(/\s+/).filter(Boolean).forEach((name) => classes.add(name)); },
    get textContent() { return text + children.map((child) => child.textContent).join(""); },
    set textContent(value) { text = String(value); children.splice(0); },
    set innerHTML(value) { text = value; children.splice(0); },
    value: "", checked: false, disabled: false,
    scrollTop: 0, clientHeight: 100, clientWidth: 100, offsetWidth: 12, offsetHeight: 16,
    get scrollHeight() { scrollHeightReads++; return 100 + children.length; },
    get scrollHeightReads() { return scrollHeightReads; },
    addEventListener(ev, f) { (listeners[ev] ||= []).push(f); },
    setAttribute(name, value) { attributes.set(name, String(value)); },
    getAttribute(name) { return attributes.get(name) ?? null; },
    setPointerCapture() {},
    getBoundingClientRect: () => ({ left: 0, top: 0, width: 410, height: 502 }),
    getContext: () => ({ createImageData: (w, h) => ({ data: new Uint8ClampedArray(w * h * 4) }), putImageData() { draws++; } }),
    get draws() { return draws; },
    append(...c) { children.push(...c); },
    replaceChildren(...c) { text = ""; children.splice(0, children.length, ...c); },
    hidden: false,
    get childElementCount() { return children.length; },
    get firstChild() { return children.length ? { remove: () => children.shift() } : null; },
  };
}

// A page loaded on mocks; `tick(ms)` moves its clock and runs what is due.
export function page(opts = {}) {
  const els = { usbdesktop: element("usbdesktop"), usbcursor: element("usbcursor") };
  els.usbdesktop.clientWidth = opts.width ?? 320;
  els.usbdesktop.clientHeight = opts.height ?? 180;
  const observers = [], hostInput = [];
  const forbiddenInput = () => { hostInput.push("input"); throw new Error("USB input reached a host input API"); };
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
      dispatchEvent: forbiddenInput,
      requestPointerLock: forbiddenInput,
    },
    window: {
      addEventListener(ev, f) { (winListeners[ev] ||= []).push(f); },
      parent: { postMessage(m) { posted.push(m); } },
      dispatchEvent: forbiddenInput,
    },
    location: { protocol: "http:", host: "127.0.0.1:8765", pathname: "/" },
    performance: { now: () => now },
    setTimeout: (f, ms = 0) => { const id = ++seq; timers.set(id, { at: now + Math.max(0, ms), f }); return id; },
    clearTimeout: (id) => timers.delete(id),
    requestAnimationFrame: (f) => { frames.push(f); return frames.length; },
    navigator: { mediaDevices: { getUserMedia: () => new Promise((resolve, reject) => media.push({ resolve, reject })) } },
    WebSocket, AudioContext, AudioWorkletNode,
    localStorage: { getItem: (k) => (store.has(k) ? store.get(k) : null), setItem: (k, v) => store.set(k, String(v)) },
    MouseEvent: forbiddenInput, PointerEvent: forbiddenInput, KeyboardEvent: forbiddenInput,
    ResizeObserver: class {
      constructor(callback) { this.callback = callback; this.targets = new Set(); observers.push(this); }
      observe(target) { this.targets.add(target); }
    },
  };
  Object.defineProperty(sandbox.navigator, "hid", { get: forbiddenInput });
  vm.createContext(sandbox);
  vm.runInContext(SRC, sandbox);

  const p = {
    sb: sandbox, els, sockets, media, micNodes, stream, nodes, modules, posted, roots, store, hostInput,
    fire(ev) { for (const f of winListeners[ev] || []) f({}); },
    slowModules() { slowModule = true; },
    get now() { return now; },
    get ws() { return sockets[sockets.length - 1]; },
    open() { p.ws.readyState = 1; return p.ws; },
    usb(snapshot) { p.ws.onmessage({ data: JSON.stringify({ usb: snapshot }) }); },
    resize(width, height) {
      els.usbdesktop.clientWidth = width; els.usbdesktop.clientHeight = height;
      for (const observer of observers) if (observer.targets.has(els.usbdesktop)) observer.callback();
    },
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

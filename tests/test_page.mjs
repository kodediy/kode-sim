// web/sim.js in node, on a mock DOM, WebSocket, clock and audio: what the
// page sends, and when. node --test tests/test_page.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { page } from "./page_harness.mjs";

const settle = () => new Promise((r) => setImmediate(r));

function hidInterface({ number, endpoint, keyboard = false, mouse = false, ids = [null], buttons = [] }) {
  return {
    number, alternate: 0, active: true, class: 3, subclass: 0, protocol: 0,
    endpoints: [{ address: endpoint, direction: "in", transfer_type: 3, attributes: 3,
                  max_packet_size: 64, transactions_per_microframe: 1, interval: 1 }],
    hid_descriptor: null, report_length: null, keyboard, mouse, buttons, hid_protocol: "report", error: "", diagnostics: [],
    reports: ids.map((report_id) => ({ report_id, length: report_id === null ? 8 : 9 })), collections: [],
  };
}
function keyboardReport(identity = {}, keys_down = [], modifiers = 0) {
  return { interface: 4, endpoint: 0x83, report_id: null, collection: 0, ...identity, keys_down, modifiers, mouse: null };
}
function mouseReport(mouse = {}, identity = {}) {
  return { interface: 7, endpoint: 0x86, report_id: null, collection: 0, ...identity, keys_down: null, modifiers: null,
           mouse: { buttons: [], relative: { x: 0, y: 0, wheel: 0 }, absolute: {}, total: { x: 0, y: 0, wheel: 0 },
                    sequence: 1, presses: {}, releases: {}, ...mouse } };
}
function usbSnapshot({ interfaces = [], reports = [], ...fields } = {}) {
  const keyboard = interfaces.some((i) => i.active && i.keyboard), mouse = interfaces.some((i) => i.active && i.mouse);
  return {
    cable: true, stage: keyboard && mouse ? "hid" : keyboard ? "keyboard" : mouse ? "mouse" : "mounted",
    error: "", log: [], read: 0, written: 0, speed: 0, last: "", layout: "es", typed: "", configuration: 1, interfaces,
    keys_down: [...new Set(reports.flatMap((r) => r.keys_down ?? []))].sort((a, b) => a - b),
    modifiers: reports.reduce((mask, r) => mask | r.modifiers, 0),
    mouse: { buttons: [...new Set(reports.flatMap((r) => r.mouse?.buttons ?? []))].sort((a, b) => a - b) },
    hid: { keyboard, mouse, reports }, ...fields,
  };
}
const keyboardInterface = () => hidInterface({ number: 4, endpoint: 0x83, keyboard: true });
const mouseInterface = () => hidInterface({ number: 7, endpoint: 0x86, mouse: true, buttons: [1, 2, 3, 4, 5] });
const mouseSnapshot = (reports = [], fields = {}) => usbSnapshot({ interfaces: [mouseInterface()], reports, ...fields });
const cursorPosition = (p) => {
  const match = p.els.usbcursor.style.transform.match(/^translate\(([^p]+)px, ([^p]+)px\)$/);
  assert.ok(match, "the virtual cursor has a DOM position");
  return match.slice(1).map(Number);
};
const mouseButton = (p, number) => p.els.usbmousebuttons.children.find((el) => el.dataset.button === String(number));

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

test("the microphone switch asks kode-sim, which turns it on for every page", async () => {
  const p = page(); p.open();
  p.els.micin.checked = true;
  p.els.micin.onchange();
  assert.deepEqual(p.ws.sent.filter((d) => typeof d === "string").map(JSON.parse), [{ mic: true }]);
  assert.equal(p.media.length, 0, "the microphone opens on kode-sim's MIC_START, not before");
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
  assert.equal(JSON.stringify(p.posted), JSON.stringify([{ kodeSim: "activity" }]), "the first press, right after loading");
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
  const base = usbSnapshot({ log: ["  0.10s  cable plugged in"] });
  say(usbSnapshot({ interfaces: [keyboardInterface()], typed: "Hola", device: { vid: "306a", pid: "1234", manufacturer: "", product: "Kode", class: "keyboard" } }));
  assert.equal(p.els.usbkbd.hidden, false);
  assert.equal(p.els.usbdisk.hidden, true);
  assert.equal(p.els.usbtyped.textContent, "Hola");
  assert.equal(p.els.usbplug.textContent, "Sacar cable");
  say({ ...base, stage: "mounted", device: { vid: "306a", pid: "1234", manufacturer: "", product: "Disk", class: "mass storage" },
        disk: { label: "KODE", free: 2 << 20, total: 4 << 20, write_protected: false, files: [{ name: "a.txt", size: 19, dir: false }] } });
  assert.equal(p.els.usbdisk.hidden, false);
  assert.equal(p.els.usbkbd.hidden, true);
  assert.equal(p.els.usbeject.hidden, false);
  assert.equal(p.els.usbfiles.children.length, 1);
  assert.match(p.els.usbvol.textContent, /KODE · 2\.0 MB libres de 4\.0 MB/);
  p.els.usbfiles.children[0].onclick();
  p.els.usbeject.onclick();
  const sent = ws.sent.filter((d) => typeof d === "string").map((d) => JSON.parse(d).usb).filter(Boolean);
  assert.deepEqual(sent, [{ cmd: "get", name: "a.txt" }, { cmd: "eject" }]);
  say({ ...base, cable: false, stage: "no_cable" });
  p.els.usbplug.onclick();
  assert.deepEqual(JSON.parse(ws.sent.at(-1)).usb, { cmd: "plug" });
});

test("USB keyboard capabilities show decoded usages, modifiers, server text and both layouts", () => {
  const p = page(); p.open();
  const interfaces = [keyboardInterface()];
  p.usb(usbSnapshot({ interfaces, reports: [keyboardReport({}, [4, 0x1234], 0x42)], typed: "Áñ", layout: "es" }));
  assert.equal(p.els.usbkbd.hidden, false);
  assert.equal(p.els.usbmouse.hidden, true);
  assert.equal(p.els.usbtyped.textContent, "Áñ");
  assert.equal(p.els.usblayout.value, "es");
  assert.match(p.els.usbkeys.textContent, /0x04, 0x1234/);
  assert.match(p.els.usbmods.textContent, /Mayús izquierda.*Alt derecho \(AltGr\).*0x42/);
  assert.match(p.els.usbkbdinterfaces.textContent, /Interfaz 4.*EP 0x83.*sin ID/);
  assert.match(p.els.usbkbdreports.textContent, /colección 0.*\nTeclas mantenidas/);
  p.els.usblayout.value = "us"; p.els.usblayout.onchange();
  p.els.usbclear.onclick();
  assert.deepEqual(p.ws.sent.map(JSON.parse), [{ usb: { cmd: "layout", layout: "us" } }, { usb: { cmd: "clear_typed" } }]);
  assert.equal(p.els.usbtyped.textContent, "Áñ", "only the server changes typed text");
  p.usb(usbSnapshot({ interfaces, reports: [keyboardReport()], typed: "", layout: "us" }));
  assert.equal(p.els.usbtyped.textContent, "");
  assert.equal(p.els.usblayout.value, "us");
  assert.match(p.els.usbkeys.textContent, /ninguna/);
  assert.match(p.els.usbmods.textContent, /ninguno \(0x00\)/);
});

test("a mouse alone shows all declared buttons before their first press", () => {
  const p = page(); p.open(); p.usb(mouseSnapshot());
  assert.equal(p.els.usbmouse.hidden, false);
  assert.equal(p.els.usbkbd.hidden, true);
  assert.equal(p.els.usbst.textContent, "ratón");
  assert.deepEqual(p.els.usbmousebuttons.children.map((el) => Number(el.dataset.button)), [1, 2, 3, 4, 5]);
  for (const el of p.els.usbmousebuttons.children) {
    assert.equal(el.dataset.held, "false");
    assert.equal(el.dataset.presses, "0");
    assert.match(el.textContent, /suelto/);
  }
  assert.deepEqual(cursorPosition(p), [(320 - 12) / 2, (180 - 16) / 2]);
  assert.equal(p.els.usbmouseevents.textContent, "Eventos nuevos: 0 · rueda relativa: 0");
});

for (const shared of [false, true]) {
  test(`keyboard and mouse remain visible with ${shared ? "shared Report IDs" : "separate interfaces"} and MSC`, () => {
    const p = page(); p.open();
    const interfaces = shared
      ? [hidInterface({ number: 9, endpoint: 0x84, keyboard: true, mouse: true, ids: [37, 193], buttons: [1, 2, 3, 7] })]
      : [mouseInterface(), keyboardInterface()];
    const k = keyboardReport(shared ? { interface: 9, endpoint: 0x84, report_id: 37 } : {}, [4], 2);
    const m = mouseReport({ buttons: [2], presses: { 2: 1 } }, shared ? { interface: 9, endpoint: 0x84, report_id: 193, collection: 1 } : {});
    const state = usbSnapshot({ interfaces, reports: [k, m], typed: "A" });
    p.usb(state);
    assert.equal(p.els.usbst.textContent, "teclado y ratón");
    assert.equal(p.els.usbkbd.hidden, false);
    assert.equal(p.els.usbmouse.hidden, false);
    assert.equal(mouseButton(p, 2).dataset.held, "true");
    assert.match(p.els.usbkbdreports.textContent, shared ? /ID 37/ : /Interfaz 4/);
    assert.match(p.els.usbmousereports.textContent, shared ? /ID 193.*colección 1/ : /Interfaz 7/);
    p.usb({ ...state, stage: "mounted", disk: { label: "KODE", free: 2 << 20, total: 4 << 20, write_protected: false,
                                                  files: [{ name: "a.txt", size: 19, dir: false }] } });
    assert.equal(p.els.usbdisk.hidden, false);
    assert.equal(p.els.usbeject.hidden, false);
    assert.equal(p.els.usbkbd.hidden, false);
    assert.equal(p.els.usbmouse.hidden, false);
    assert.equal(p.els.usbtyped.textContent, "A");
    p.els.usbfiles.children[0].onclick(); p.els.usbeject.onclick();
    assert.deepEqual(p.ws.sent.map(JSON.parse), [{ usb: { cmd: "get", name: "a.txt" } }, { usb: { cmd: "eject" } }]);
  });
}

test("keyboard interfaces keep independent held keys and modifiers", () => {
  const p = page(); p.open();
  const interfaces = [keyboardInterface(), hidInterface({ number: 2, endpoint: 0x85, keyboard: true, ids: [61] })];
  const second = keyboardReport({ interface: 2, endpoint: 0x85, report_id: 61 }, [4, 5], 0x10);
  p.usb(usbSnapshot({ interfaces, reports: [keyboardReport({}, [4], 2), second] }));
  assert.equal(p.els.usbkbdreports.children.length, 2);
  assert.match(p.els.usbmods.textContent, /Mayús izquierda.*Ctrl derecho/);
  assert.match(p.els.usbkbdinterfaces.textContent, /Interfaz 4.*Interfaz 2/);
  p.usb(usbSnapshot({ interfaces, reports: [keyboardReport(), second] }));
  assert.match(p.els.usbkeys.textContent, /0x04, 0x05/);
  assert.doesNotMatch(p.els.usbmods.textContent, /Mayús/);
  assert.match(p.els.usbkbdreports.children[0].textContent, /ninguna/);
  assert.match(p.els.usbkbdreports.children[1].textContent, /0x04, 0x05/);
});

test("relative totals move in both directions and repeated identical reports move twice", () => {
  const p = page(); p.open(); p.usb(mouseSnapshot());
  const origin = cursorPosition(p);
  const relative = { x: 6, y: -4, wheel: 1 };
  p.usb(mouseSnapshot([mouseReport({ relative, total: relative })]));
  assert.deepEqual(cursorPosition(p), [origin[0] + 6, origin[1] - 4]);
  p.usb(mouseSnapshot([mouseReport({ relative, total: { x: 12, y: -8, wheel: 2 }, sequence: 2 })]));
  assert.deepEqual(cursorPosition(p), [origin[0] + 12, origin[1] - 8]);
  assert.equal(p.els.usbmouseevents.textContent, "Eventos nuevos: 2 · rueda relativa: 2");
  p.usb(mouseSnapshot([mouseReport({ relative: { x: -15, y: 10, wheel: -3 }, total: { x: -3, y: 2, wheel: -1 }, sequence: 3 })]));
  assert.deepEqual(cursorPosition(p), [origin[0] - 3, origin[1] + 2]);
  assert.equal(p.els.usbmouseevents.textContent, "Eventos nuevos: 3 · rueda relativa: -1");
  assert.match(p.els.usbmousereports.textContent, /Último relativo: X -15 · Y 10 · WHEEL -3/);
});

test("duplicate snapshots and keyboard-only updates never replay mouse motion or button edges", () => {
  const p = page(); p.open();
  const interfaces = [keyboardInterface(), mouseInterface()];
  p.usb(usbSnapshot({ interfaces }));
  const mouse = mouseReport({ relative: { x: 10, y: 5, wheel: 2 }, total: { x: 30, y: 15, wheel: 6 }, sequence: 3,
                             presses: { 1: 1 }, releases: { 1: 1 } });
  const state = usbSnapshot({ interfaces, reports: [mouse] });
  p.usb(state);
  const position = cursorPosition(p), events = p.els.usbmouseevents.textContent, last = p.els.usbmousereports.textContent;
  for (let i = 0; i < 4; i++) p.usb({ ...state, log: [String(i)] });
  p.usb(usbSnapshot({ interfaces, reports: [keyboardReport({}, [5], 0x40), mouse], typed: "b" }));
  assert.deepEqual(cursorPosition(p), position);
  assert.equal(p.els.usbmouseevents.textContent, events);
  assert.equal(p.els.usbmousereports.textContent, last);
  assert.equal(mouseButton(p, 1).dataset.presses, "1");
  assert.equal(mouseButton(p, 1).dataset.releases, "1");
});

test("a complete quick click survives one publication with its button currently released", () => {
  const p = page(); p.open(); p.usb(mouseSnapshot());
  p.usb(mouseSnapshot([mouseReport({ sequence: 2, presses: { 2: 1 }, releases: { 2: 1 } })]));
  const button = mouseButton(p, 2);
  assert.equal(button.dataset.held, "false");
  assert.equal(button.classList.contains("held"), false);
  assert.equal(button.dataset.presses, "1");
  assert.equal(button.dataset.releases, "1");
  assert.match(button.textContent, /Derecho: suelto.*pulsaciones nuevas 1.*liberaciones nuevas 1/);
  assert.match(p.els.usbmousereports.textContent, /pulsaciones: Derecho \+1 · liberaciones: Derecho \+1/);
  p.usb(mouseSnapshot([mouseReport({ sequence: 6, presses: { 2: 3 }, releases: { 2: 3 } })]));
  assert.equal(mouseButton(p, 2).dataset.presses, "3");
  assert.equal(mouseButton(p, 2).dataset.releases, "3");
});

test("left, right, middle and sparse extra buttons show current holds separately from transitions", () => {
  const p = page(); p.open();
  const interfaces = [hidInterface({ number: 7, endpoint: 0x86, mouse: true, buttons: [1, 2, 3, 7] })];
  const say = (report) => p.usb(usbSnapshot({ interfaces, reports: [report] }));
  p.usb(usbSnapshot({ interfaces }));
  say(mouseReport({ buttons: [1, 2, 3, 7], presses: { 1: 1, 2: 1, 3: 1, 7: 1 } }));
  for (const number of [1, 2, 3, 7]) {
    assert.equal(mouseButton(p, number).dataset.held, "true");
    assert.equal(mouseButton(p, number).classList.contains("held"), true);
    assert.equal(mouseButton(p, number).dataset.presses, "1");
  }
  assert.equal(mouseButton(p, 4), undefined, "no undeclared button is invented");
  assert.match(mouseButton(p, 7).textContent, /Botón 7/);
  say(mouseReport({ sequence: 2, presses: { 1: 1, 2: 1, 3: 1, 7: 1 }, releases: { 1: 1, 2: 1, 3: 1, 7: 1 } }));
  for (const number of [1, 2, 3, 7]) {
    assert.equal(mouseButton(p, number).dataset.held, "false");
    assert.equal(mouseButton(p, number).dataset.releases, "1");
  }
});

for (const [field, value] of [["interface", 2], ["endpoint", 0x85], ["report_id", 149], ["collection", 3]]) {
  test(`mouse counters are scoped by ${field}`, () => {
    const p = page(); p.open(); p.usb(mouseSnapshot());
    const origin = cursorPosition(p);
    const first = mouseReport({ sequence: 10, total: { x: 10 }, relative: { x: 1 }, buttons: [1], presses: { 1: 1 } });
    p.usb(mouseSnapshot([first]));
    const second = mouseReport({ total: { x: 7 }, relative: { x: 7 }, buttons: [1], presses: { 1: 1 } }, { [field]: value });
    p.usb(mouseSnapshot([second, first]));
    assert.deepEqual(cursorPosition(p), [origin[0] + 17, origin[1]]);
    assert.equal(p.els.usbmousereports.children.length, 2);
    assert.equal(mouseButton(p, 1).dataset.presses, "2");
    const released = { ...first, mouse: { ...first.mouse, sequence: 11, buttons: [], releases: { 1: 1 } } };
    p.usb(mouseSnapshot([released, second]));
    assert.equal(mouseButton(p, 1).dataset.held, "true", "another source still holds the button");
    assert.equal(mouseButton(p, 1).dataset.releases, "1");
    assert.deepEqual(cursorPosition(p), [origin[0] + 17, origin[1]]);
  });
}

test("absolute axes stay visible as absolute values and do not move the relative cursor", () => {
  const p = page(); p.open(); p.usb(mouseSnapshot());
  const origin = cursorPosition(p);
  p.usb(mouseSnapshot([mouseReport({ relative: {}, total: {}, absolute: { x: 65535, y: 200, wheel: 10 } })]));
  assert.deepEqual(cursorPosition(p), origin);
  assert.match(p.els.usbmousereports.textContent, /absoluto: X 65535 · Y 200 · WHEEL 10/);
  assert.equal(p.els.usbmouseevents.textContent, "Eventos nuevos: 1 · rueda relativa: 0");
  p.usb(mouseSnapshot([mouseReport({ sequence: 2, relative: {}, total: {}, absolute: { x: 3, y: 400, wheel: 2 } })]));
  assert.deepEqual(cursorPosition(p), origin);
});

test("a late WebSocket client synchronizes held state without replaying old movement or clicks", () => {
  const p = page(); p.open();
  const old = mouseReport({ sequence: 100, relative: { x: 20, y: -10, wheel: 4 }, total: { x: 800, y: -500, wheel: 90 },
                           buttons: [3], presses: { 1: 7, 3: 2 }, releases: { 1: 7, 3: 1 } });
  p.usb(mouseSnapshot([old]));
  const origin = cursorPosition(p);
  assert.deepEqual(origin, [(320 - 12) / 2, (180 - 16) / 2]);
  assert.equal(mouseButton(p, 3).dataset.held, "true");
  assert.equal(mouseButton(p, 1).dataset.presses, "0");
  assert.equal(p.els.usbmouseevents.textContent, "Eventos nuevos: 0 · rueda relativa: 0");
  p.usb(mouseSnapshot([old]));
  assert.deepEqual(cursorPosition(p), origin);
  p.usb(mouseSnapshot([{ ...old, mouse: { ...old.mouse, sequence: 101, total: { x: 803, y: -502, wheel: 91 } } }]));
  assert.deepEqual(cursorPosition(p), [origin[0] + 3, origin[1] - 2]);
  assert.equal(p.els.usbmouseevents.textContent, "Eventos nuevos: 1 · rueda relativa: 1");
});

test("reconnection clears transient state, preserves text and baselines the current USB snapshot", () => {
  const p = page(); p.open();
  const interfaces = [keyboardInterface(), mouseInterface()];
  p.usb(usbSnapshot({ interfaces, typed: "conservado" }));
  const reports = [keyboardReport({}, [4], 2), mouseReport({ sequence: 4, total: { x: 30 }, relative: { x: 10 },
                                                         buttons: [1], presses: { 1: 1 } })];
  p.usb(usbSnapshot({ interfaces, reports, typed: "conservado" }));
  p.ws.readyState = 3; p.ws.onclose();
  assert.equal(p.els.usbkbd.hidden, true);
  assert.equal(p.els.usbmouse.hidden, true);
  assert.equal(p.els.usbcursor.hidden, true);
  assert.equal(p.els.usbcursor.style.transform, "");
  assert.match(p.els.usbkeys.textContent, /ninguna/);
  assert.equal(p.els.usbmousebuttons.children.length, 0);
  assert.equal(p.els.usbtyped.textContent, "conservado");
  p.tick(1000); p.open();
  p.usb(usbSnapshot({ interfaces, reports, typed: "conservado" }));
  assert.deepEqual(cursorPosition(p), [(320 - 12) / 2, (180 - 16) / 2]);
  assert.equal(mouseButton(p, 1).dataset.held, "true");
  assert.equal(mouseButton(p, 1).dataset.presses, "0");
  assert.equal(p.els.usbmouseevents.textContent, "Eventos nuevos: 0 · rueda relativa: 0");
});

test("reset counters synchronize a new mouse session without negative movement or false clicks", () => {
  const p = page(); p.open(); p.usb(mouseSnapshot());
  p.usb(mouseSnapshot([mouseReport({ sequence: 9, total: { x: 40, y: 10, wheel: 8 }, buttons: [1], presses: { 1: 5 }, releases: { 1: 4 } })]));
  p.usb(mouseSnapshot([mouseReport({ sequence: 1, total: { x: -50, y: -10, wheel: -8 }, presses: {}, releases: {} })]));
  const origin = cursorPosition(p);
  assert.deepEqual(origin, [(320 - 12) / 2, (180 - 16) / 2]);
  assert.equal(mouseButton(p, 1).dataset.held, "false");
  assert.equal(mouseButton(p, 1).dataset.presses, "0");
  assert.equal(p.els.usbmouseevents.textContent, "Eventos nuevos: 0 · rueda relativa: 0");
  p.usb(mouseSnapshot([mouseReport({ sequence: 2, total: { x: -48, y: -7, wheel: -7 } })]));
  assert.deepEqual(cursorPosition(p), [origin[0] + 2, origin[1] + 3]);
});

test("endpoint closure removes that source and closing all sources clears the virtual session", () => {
  const p = page(); p.open(); p.usb(mouseSnapshot());
  const a = mouseReport({ total: { x: 10 }, buttons: [1], presses: { 1: 1 } });
  const b = mouseReport({ total: { y: 5 }, buttons: [2], presses: { 2: 1 } }, { report_id: 193 });
  p.usb(mouseSnapshot([a, b]));
  const position = cursorPosition(p);
  p.usb(mouseSnapshot([b]));
  assert.equal(mouseButton(p, 1).dataset.held, "false");
  assert.equal(mouseButton(p, 2).dataset.held, "true");
  assert.equal(p.els.usbmousereports.children.length, 1);
  assert.deepEqual(cursorPosition(p), position);
  p.usb(mouseSnapshot());
  assert.deepEqual(cursorPosition(p), [(320 - 12) / 2, (180 - 16) / 2]);
  assert.equal(p.els.usbmouseevents.textContent, "Eventos nuevos: 0 · rueda relativa: 0");
  p.usb(mouseSnapshot([mouseReport({ total: { x: 2 } })]));
  assert.deepEqual(cursorPosition(p), [(320 - 12) / 2 + 2, (180 - 16) / 2]);
});

for (const stage of ["no_cable", "waiting", "enumerating", "failed"]) {
  test(`USB ${stage} clears HID session state and retains the server's typed text`, () => {
    const p = page(); p.open();
    const interfaces = [keyboardInterface(), mouseInterface()];
    p.usb(usbSnapshot({ interfaces, typed: "texto" }));
    p.usb(usbSnapshot({ interfaces, typed: "texto", reports: [keyboardReport({}, [4], 2), mouseReport({ total: { x: 10 }, buttons: [1], presses: { 1: 1 } })] }));
    p.usb(usbSnapshot({ cable: stage !== "no_cable", stage, configuration: 0, typed: "texto" }));
    assert.equal(p.els.usbkbd.hidden, true);
    assert.equal(p.els.usbmouse.hidden, true);
    assert.equal(p.els.usbcursor.hidden, true);
    assert.equal(p.els.usbmousereports.children.length, 0);
    assert.equal(p.els.usbcursor.style.transform, "");
    assert.equal(p.els.usbtyped.textContent, "texto");
    p.usb(mouseSnapshot([mouseReport({ total: { x: 2 } })], { typed: "texto" }));
    assert.deepEqual(cursorPosition(p), [(320 - 12) / 2 + 2, (180 - 16) / 2]);
    assert.equal(p.els.usbmouseevents.textContent, "Eventos nuevos: 1 · rueda relativa: 0");
  });
}

test("firmware off, crash, HELLO, POWER_OFF and a busy socket clear HID state", () => {
  for (const trigger of ["off", "crashed", "busy", 0x01, 0x40]) {
    const p = page(); p.open();
    p.usb(mouseSnapshot([], { typed: "texto" }));
    p.usb(mouseSnapshot([mouseReport({ total: { x: 8 }, buttons: [1], presses: { 1: 1 } })], { typed: "texto" }));
    if (typeof trigger === "string") p.ws.onmessage({ data: JSON.stringify({ state: trigger }) });
    else p.sb.onFrame(trigger, new ArrayBuffer(0), 0, 0);
    if (trigger === "busy") p.ws.onclose();
    assert.equal(p.els.usbmouse.hidden, true);
    assert.equal(p.els.usbcursor.hidden, true);
    assert.equal(p.els.usbmousebuttons.children.length, 0);
    assert.equal(p.els.usbtyped.textContent, "texto");
  }
});

test("the cursor clamps to measured DOM bounds and resizes on small and embedded screens", () => {
  for (const embed of [false, true]) {
    const p = page({ embed, width: 217, height: 103 }); p.open(); p.usb(mouseSnapshot());
    p.usb(mouseSnapshot([mouseReport({ total: { x: 10000, y: 10000 } })]));
    assert.deepEqual(cursorPosition(p), [217 - 12, 103 - 16]);
    p.resize(91, 57);
    assert.deepEqual(cursorPosition(p), [91 - 12, 57 - 16]);
    p.usb(mouseSnapshot([mouseReport({ sequence: 2, total: { x: -10000, y: -10000 } })]));
    assert.deepEqual(cursorPosition(p), [0, 0]);
    p.resize(8, 7);
    assert.deepEqual(cursorPosition(p), [0, 0]);
  }
});

test("mouse motion received while the USB panel has no layout is consumed once it becomes visible", () => {
  const p = page({ embed: true, width: 0, height: 0 }); p.open(); p.usb(mouseSnapshot());
  p.usb(mouseSnapshot([mouseReport({ total: { x: 6, y: -4 } })]));
  assert.equal(p.els.usbcursor.hidden, true);
  p.resize(230, 118);
  const position = [(230 - 12) / 2 + 6, (118 - 16) / 2 - 4];
  assert.deepEqual(cursorPosition(p), position);
  p.resize(230, 118);
  assert.deepEqual(cursorPosition(p), position);
});

test("non-HID messages preserve USB state and mouse visualization never injects host input or draws on AMOLED", () => {
  const p = page(); p.open(); p.usb(mouseSnapshot());
  p.usb(mouseSnapshot([mouseReport({ total: { x: 6 }, buttons: [1], presses: { 1: 1 } })]));
  const position = cursorPosition(p);
  p.ws.onmessage({ data: JSON.stringify({ console: "USB is active", battery: { percent: 80, usb: true }, sd: true }) });
  p.sb.onFrame(0xfe, new ArrayBuffer(0), 0, 0);
  assert.deepEqual(cursorPosition(p), position);
  assert.deepEqual(p.ws.sent, []);
  assert.deepEqual(p.hostInput, []);
  assert.equal(p.els.screen.draws, 0);
  assert.equal(p.els.usbdesktop.listeners.pointermove, undefined);
  assert.equal(p.els.usbdesktop.listeners.pointerdown, undefined);
  p.usb(usbSnapshot({ stage: "mounted", typed: "conservado" }));
  assert.equal(p.els.usbmouse.hidden, true);
  assert.equal(p.els.usbtyped.textContent, "conservado");
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
  const q = page({ stored: { "kode-sim.mic": "off" } }); const w2 = q.open(); w2.onopen();
  assert.ok(!w2.sent.some((d) => typeof d === "string" && d.includes('"mic":true')), w2.sent);
});

test("turning the microphone off is remembered", () => {
  const p = page(); p.open();
  p.els.micin.checked = false; p.els.micin.onchange();
  assert.equal(p.store.get("kode-sim.mic"), "off");
  p.els.micin.checked = true; p.els.micin.onchange();
  assert.equal(p.store.get("kode-sim.mic"), "on");
});

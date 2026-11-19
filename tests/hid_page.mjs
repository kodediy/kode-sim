import assert from "node:assert/strict";
import { page } from "./page_harness.mjs";

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const states = JSON.parse(Buffer.concat(chunks).toString());
assert.ok(states.length);
const identity = (r) => JSON.stringify([r.interface, r.endpoint, r.report_id, r.collection]);
const sources = (u) => new Map(u.hid.reports.filter((r) => r.mouse !== null).map((r) => [identity(r), r.mouse]));
const position = (p) => p.els.usbcursor.style.transform.match(/-?[\d.]+(?=px)/g).map(Number);

for (const options of [{}, { embed: true, width: 120, height: 80 }]) {
  const p = page(options);
  p.open();
  let previous = null;
  for (const u of states) {
    const before = previous?.hid.mouse ? position(p) : null;
    p.usb(u);
    assert.equal(p.els.usbtyped.textContent, u.typed);
    assert.equal(p.els.usblayout.value, u.layout);
    assert.equal(p.els.usbkbd.hidden, !u.hid.keyboard);
    assert.equal(p.els.usbmouse.hidden, !u.hid.mouse);
    if (u.disk && u.stage === "mounted") {
      assert.equal(p.els.usbdisk.hidden, false);
      assert.equal(p.els.usbfiles.children.length, u.disk.files.length);
      for (const file of u.disk.files) assert.ok(p.els.usbfiles.textContent.includes(file.name));
    }
    if (u.hid.keyboard) {
      for (const key of u.keys_down) assert.ok(p.els.usbkeys.textContent.includes(`0x${key.toString(16).padStart(2, "0")}`));
      assert.ok(p.els.usbmods.textContent.includes(`0x${u.modifiers.toString(16).padStart(2, "0")}`));
      assert.equal(p.els.usbkbdreports.children.length, u.hid.reports.filter((r) => r.keys_down !== null).length);
    }
    if (u.hid.mouse) {
      const current = sources(u);
      const older = previous ? sources(previous) : new Map();
      for (const [key, mouse] of current) {
        const readout = p.els.usbmousereports.children.find((el) => el.dataset.source === key);
        assert.ok(readout, key);
        assert.ok(readout.textContent.includes(`eventos USB: ${mouse.sequence}`));
        for (const [axis, total] of Object.entries(mouse.total)) assert.ok(readout.textContent.includes(`${axis.toUpperCase()} ${total}`));
      }
      for (const button of p.els.usbmousebuttons.children) {
        assert.equal(button.dataset.held, String(u.mouse.buttons.includes(Number(button.dataset.button))));
      }
      if (before) {
        const delta = (axis) => [...current].reduce((sum, [key, mouse]) =>
          sum + (mouse.total[axis] ?? 0) - (older.get(key)?.total[axis] ?? 0), 0);
        const maximum = [p.els.usbdesktop.clientWidth - p.els.usbcursor.offsetWidth,
                         p.els.usbdesktop.clientHeight - p.els.usbcursor.offsetHeight];
        assert.deepEqual(position(p), ["x", "y"].map((axis, i) => Math.max(0, Math.min(maximum[i], before[i] + delta(axis)))));
      }
    }
    const observed = [p.els.usbcursor.style.transform, p.els.usbmouseevents.textContent,
                      p.els.usbmousebuttons.textContent, p.els.usbtyped.textContent];
    p.usb(u);
    assert.deepEqual([p.els.usbcursor.style.transform, p.els.usbmouseevents.textContent,
                      p.els.usbmousebuttons.textContent, p.els.usbtyped.textContent], observed);
    assert.deepEqual(p.hostInput, []);
    assert.equal(p.els.screen.draws, 0);
    previous = u;
  }
}

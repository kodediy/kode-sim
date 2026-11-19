"""The USB cable, from a page: the computer on it plugs in, enumerates what
answers (the fake esp-emu is a boot keyboard) and shows what it types."""

import json

import pytest

import kd
from sim import next_json, ws, start


def usb(j):
    return "usb" in j


async def _send(w, cmd):
    await w.send_str(json.dumps({"usb": cmd}))


async def test_a_page_plugs_the_cable_in_and_the_computer_enumerates_the_keyboard(sim):
    w = await ws(sim)
    first = await next_json(w, usb)
    assert first["usb"]["stage"] == "no_cable"
    await _send(w, {"cmd": "plug"})
    u = (await next_json(w, lambda j: usb(j) and j["usb"]["stage"] == "keyboard"))["usb"]
    assert u["cable"] and u["device"]["class"] == "keyboard" and u["device"]["product"] == "Fake"
    assert u["device"]["vid"] == "306a"
    await w.close()
    await w.session.close()


async def test_what_the_keyboard_reports_is_typed_on_every_page_and_a_late_one_is_told(sim):
    a = await ws(sim)
    await _send(a, {"cmd": "plug"})
    await next_json(a, lambda j: usb(j) and j["usb"]["stage"] == "keyboard")
    await a.send_str(json.dumps({"console_in": "key 2 11"}))   # shift + h
    await a.send_str(json.dumps({"console_in": "key 0 12"}))   # i
    u = (await next_json(a, lambda j: usb(j) and j["usb"]["typed"] == "Hi"))["usb"]
    assert u["layout"] == "es"
    late = await ws(sim)
    told = (await next_json(late, usb))["usb"]
    assert told["typed"] == "Hi" and told["stage"] == "keyboard", "a page that joins late is sent the cable's state"
    for w in (a, late):
        await w.close()
        await w.session.close()


async def test_pulling_the_cable_out_forgets_the_device(sim):
    w = await ws(sim)
    await _send(w, {"cmd": "plug"})
    await next_json(w, lambda j: usb(j) and j["usb"]["stage"] == "keyboard")
    await _send(w, {"cmd": "unplug"})
    u = (await next_json(w, lambda j: usb(j) and j["usb"]["stage"] == "no_cable"))["usb"]
    assert not u["cable"] and "device" not in u
    await w.close()
    await w.session.close()


async def test_a_command_it_does_not_know_does_nothing(sim):
    w = await ws(sim)
    await _send(w, {"cmd": "format_everything"})
    await _send(w, {"cmd": "put", "name": "a", "data": "not base64!"})
    await _send(w, {"cmd": "plug"})
    u = (await next_json(w, lambda j: usb(j) and j["usb"]["cable"]))["usb"]
    assert u["stage"] in ("waiting", "enumerating", "keyboard")
    await w.close()
    await w.session.close()


@pytest.fixture(params=["keyboard", "mouse", "separate", "shared"])
async def hid_sim(request, exe, fake_emu, tmp_path, monkeypatch):
    monkeypatch.setenv("FAKE_USB_MODE", request.param)
    running = await start(exe, tmp_path)
    yield request.param, running
    running.stop_clean()


async def test_hid_interfaces_events_late_snapshot_and_lifecycle(hid_sim):
    mode, running = hid_sim
    keyboard = mode != "mouse"
    mouse = mode != "keyboard"
    stage = "hid" if keyboard and mouse else "keyboard" if keyboard else "mouse"
    w = await ws(running)
    late = None
    try:
        await next_json(w, usb)
        await _send(w, {"cmd": "plug"})
        u = (await next_json(w, lambda j: usb(j) and j["usb"]["stage"] == stage))["usb"]
        assert u["hid"]["keyboard"] == keyboard
        assert u["hid"]["mouse"] == mouse
        assert len(u["interfaces"]) == (2 if mode == "separate" else 1)
        assert all(i["hid_protocol"] == "report" and not i["error"] for i in u["interfaces"])
        if mode in ("shared", "separate"):
            assert u["device"]["class"] == "composite"
        if mode == "shared":
            ids = [r["report_id"] for r in u["interfaces"][0]["reports"]]
            assert len(ids) == 2 and len(set(ids)) == 2 and all(ids)
        if keyboard:
            for _ in range(2):
                await w.send_str(json.dumps({"console_in": "key 2 4"}))
        if mouse:
            for _ in range(2):
                await w.send_str(json.dumps({"console_in": "mouse 5 6 -4 1"}))
        def received(j):
            if not usb(j):
                return False
            state = j["usb"]
            if keyboard and (state["typed"] != "A" or state["keys_down"] != [4]):
                return False
            mice = [r["mouse"] for r in state["hid"]["reports"] if r["mouse"] is not None]
            return not mouse or mice and mice[0]["total"]["x"] == 12
        u = (await next_json(w, received))["usb"]
        assert u["modifiers"] == (2 if keyboard else 0)
        assert u["mouse"]["buttons"] == ([1, 3] if mouse else [])
        if mouse:
            event = next(r for r in u["hid"]["reports"] if r["mouse"] is not None)
            assert event["mouse"]["relative"] == {"x": 6, "y": -4, "wheel": 1}
            assert event["mouse"]["total"] == {"x": 12, "y": -8, "wheel": 2}
            assert event["mouse"]["sequence"] == 2
        late = await ws(running)
        current = (await next_json(late, usb))["usb"]
        for field in ("interfaces", "hid", "keys_down", "modifiers", "mouse", "typed"):
            assert current[field] == u[field]
        if mouse:
            await w.send_str(json.dumps({"console_in": "mouse 0 0 0 0"}))
            u = (await next_json(w, lambda j: usb(j) and j["usb"]["mouse"]["buttons"] == []))["usb"]
            assert u["keys_down"] == ([4] if keyboard else [])
            event = next(r["mouse"] for r in u["hid"]["reports"] if r["mouse"] is not None)
            assert event["presses"] == {"1": 1, "3": 1}
            assert event["releases"] == {"1": 1, "3": 1}
            # Both edges can occur within the publication interval.
            await w.send_str(json.dumps({"console_in": "mouse 2 0 0 0"}))
            await w.send_str(json.dumps({"console_in": "mouse 0 0 0 0"}))
            def clicked(j):
                if not usb(j):
                    return False
                mice = [r["mouse"] for r in j["usb"]["hid"]["reports"] if r["mouse"] is not None]
                return mice and mice[0]["presses"].get("2") == 1 and mice[0]["releases"].get("2") == 1
            await next_json(w, clicked)
        await _send(w, {"cmd": "reset"})
        u = (await next_json(w, lambda j: usb(j) and j["usb"]["stage"] == stage
                            and j["usb"]["hid"]["reports"] == []))["usb"]
        assert u["keys_down"] == [] and u["modifiers"] == 0 and u["mouse"]["buttons"] == []
        await _send(w, {"cmd": "unplug"})
        u = (await next_json(w, lambda j: usb(j) and j["usb"]["stage"] == "no_cable"))["usb"]
        assert not u["cable"] and "device" not in u and u["interfaces"] == []
        assert u["hid"] == {"keyboard": False, "mouse": False, "reports": []}
    finally:
        for socket in (w, late):
            if socket is not None:
                await socket.close()
                await socket.session.close()

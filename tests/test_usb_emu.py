"""USB through a real P4 firmware, ESP-TinyUSB 2.2.0 and the real esp-emu.

No fake_emu fixture is used. The direct variant links the same components
but owns its task/descriptors, detecting accidental extraction of ESP's API.
"""
import asyncio
import base64
import json
import subprocess
import re
import shlex
import shutil
import zlib
from pathlib import Path

import pytest

import kd
from emu_helpers import boot, console_line, firmware, no_panic
from conftest import CACHE
from sim import next_json, ws

pytestmark = pytest.mark.emu
FIXTURE = Path(__file__).parent / "fixtures" / "usb"


@pytest.fixture(scope="module", params=[(False, None), (True, None), (True, "0.19.0~3")],
                ids=["esp_tinyusb", "direct", "direct_0_19"])
def usb_firmware(request, exe, kode_sdk):
    direct, version = request.param
    name = (f"direct_{version}" if version else "direct") if direct else "esp_tinyusb"
    source = CACHE / "usb-fixtures" / name
    if source.exists():
        shutil.rmtree(source)
    shutil.copytree(FIXTURE, source)
    if direct:
        with (source / "sdkconfig.defaults").open("a") as f:
            f.write("\nCONFIG_USB_TEST_DIRECT=y\n")
    if version:
        with (source / "main" / "idf_component.yml").open("a") as f:
            f.write(f'  espressif/tinyusb: "=={version}"\n')
    fw = firmware(exe, source, sdk=kode_sdk)
    verify_linkage(fw, direct)
    return fw, direct


def component_versions(fw):
    description = json.loads((fw["elf"].parent / "project_description.json").read_text())
    lock = (Path(description["project_path"]) / "dependencies.lock").read_text()
    versions = {}
    for name in ("idf", "kodediy/kode", "espressif/esp_tinyusb", "espressif/tinyusb"):
        section = re.search(rf"^  {re.escape(name)}:\n(.*?)(?=^  \S|\Z)", lock, re.M | re.S)
        assert section, f"{name} absent from the resolved lockfile"
        versions[name] = re.search(r"^    version: ['\"]?([^'\"\s]+)", section[1], re.M)[1]
    assert versions["espressif/esp_tinyusb"] == "2.2.0", versions
    print("USB resolved versions " + json.dumps(versions, sort_keys=True))
    return versions


def verify_linkage(fw, direct):
    """Check the produced binary, including the weak same-object DCD call."""
    cache = (fw["elf"].parent / "CMakeCache.txt").read_text()
    def tool(name):
        match = re.search(rf"^CMAKE_{name}:[^=]+=(.+)$", cache, re.M)
        assert match, f"CMAKE_{name} missing from the build cache"
        return match[1]
    symbols = subprocess.check_output([tool("NM"), "-g", str(fw["elf"])], text=True)
    assert re.search(r" T dcd_deinit$", symbols, re.M), "TinyUSB's weak DCD deinit was not overridden"
    disasm = subprocess.check_output([tool("OBJDUMP"), "-d", "--disassemble=tud_deinit", str(fw["elf"])], text=True)
    assert "<dcd_deinit>" in disasm, disasm
    assert "<__wrap_dcd_int_disable>" in disasm, disasm
    event = subprocess.check_output([tool("OBJDUMP"), "-d", "--disassemble=dcd_event_handler", str(fw["elf"])], text=True)
    assert "<__wrap_xQueueGenericSend>" in event, event
    assert not re.search(r" T dcd_init$", symbols, re.M), "Hardware DWC2 driver was linked"
    task = subprocess.check_output([tool("OBJDUMP"), "-d", "--disassemble=__wrap_tud_task_ext", str(fw["elf"])], text=True)
    assert "<tud_task_ext>" in task, task
    assert re.search(r" T __wrap_vTaskDelete$", symbols, re.M), "Consumer deletion barrier not linked"
    if not direct:
        stop = subprocess.check_output([tool("OBJDUMP"), "-d", "--disassemble=tinyusb_task_stop", str(fw["elf"])], text=True)
        assert "<__wrap_vTaskDelete>" in stop, stop
    real_driver = bool(re.search(r" T tinyusb_driver_install$", symbols, re.M))
    assert real_driver != direct, "ESP-TinyUSB must link only for users of its API"
    if not direct:
        assert re.search(r" T tinyusb_task_start$", symbols, re.M), "Official ESP-TinyUSB task was not linked"


def queue_depth(fw):
    # Read the exact TinyUSB translation unit's preprocessor configuration;
    # its queue default is private to usbd.c and is not an application macro.
    commands = json.loads((fw["elf"].parent / "compile_commands.json").read_text())
    entry = next(c for c in commands if Path(c["file"]).name == "usbd.c")
    args = shlex.split(entry["command"])
    output = args.index("-o")
    del args[output:output + 2]
    args.remove("-c")
    defines = subprocess.check_output([*args, "-E", "-dM"], cwd=entry["directory"], text=True)
    value = re.search(r"^#define CFG_TUD_TASK_QUEUE_SZ (.+)$", defines, re.M)
    assert value, "TinyUSB event queue configuration not found"
    return int(value[1], 0)


async def result(k, command, expected="ESP_OK", timeout=90):
    """Only the answer to this command, including repeated identical commands."""
    answer = asyncio.get_running_loop().create_future()
    lines = []

    def receive(line):
        lines.append(line)
        if line.startswith(f"USB result {command} ") and not answer.done():
            answer.set_result(line)

    k.on_line.append(receive)
    try:
        await k.console_in(command)
        line = await asyncio.wait_for(answer, timeout)
        assert line == f"USB result {command} {expected}", "\n".join(lines)
        return lines
    finally:
        k.on_line.remove(receive)


async def hid_report(k, command, expected="ESP_OK"):
    """The next report may share this endpoint: wait for TinyUSB to finish it."""
    complete = asyncio.get_running_loop().create_future()

    def receive(line):
        if re.fullmatch(r"USB report complete instance=\d+ length=\d+", line) and not complete.done():
            complete.set_result(line)

    k.on_line.append(receive)
    try:
        lines = await result(k, command, expected)
        if expected == "ESP_OK":
            await asyncio.wait_for(complete, 90)
        return lines
    finally:
        k.on_line.remove(receive)


async def cable(w, command, stage):
    await w.send_str(json.dumps({"usb": {"cmd": command}}))
    return (await next_json(w, lambda j: j.get("usb", {}).get("stage") == stage, timeout=120))["usb"]


async def usb_resources(k):
    lines = await result(k, "resources")
    measured = [re.fullmatch(r"USB resources heap=(\d+) tasks=(\d+)", line) for line in lines]
    values = [tuple(map(int, match.groups())) for match in measured if match]
    assert len(values) == 1, lines
    print(f"USB resources heap={values[0][0]} tasks={values[0][1]}")
    return values[0]


async def keyboard(w):
    u = (await next_json(w, lambda j: j.get("usb", {}).get("stage") == "keyboard", timeout=120))["usb"]
    assert u["cable"] and not u["error"]
    assert u["device"] == {"vid": "303a", "pid": "4004", "manufacturer": "kode-sim",
                           "product": "ESP-TinyUSB lifecycle", "class": "keyboard"}
    assert any("SET_CONFIGURATION 1" in line and "0x81" in line for line in u["log"])
    return u


async def typed(k, w):
    await w.send_str(json.dumps({"usb": {"cmd": "clear_typed"}}))
    await next_json(w, lambda j: j.get("usb", {}).get("typed") == "")
    await result(k, "report")
    await next_json(w, lambda j: j.get("usb", {}).get("typed") == "a", timeout=60)


async def test_real_stack_enumerates_reconnects_and_reinstalls(exe, usb_firmware, tmp_path):
    fw, direct = usb_firmware
    k = await boot(exe, tmp_path, fw)
    w = None
    try:
        await console_line(k, r"USB result install ESP_OK", 120)
        versions = await console_line(k, r"USB versions idf=v6\.1(?:[ .-]|$)", 30)
        resolved = component_versions(fw)["espressif/tinyusb"].split("~", 1)[0]
        assert f"tinyusb={resolved}" in versions, versions
        port_line = await console_line(k, r"USB port TINYUSB_PORT_FULL_SPEED_0=\d+", 30)
        port = re.search(r"=(\d+)", port_line)[1]
        w = await ws(k.sim)
        await w.send_str(json.dumps({"usb": {"cmd": "plug"}}))
        await keyboard(w)
        await console_line(k, rf"USB attached 1 rhport={port} speed=full configuration=1", 30)
        await typed(k, w)
        before = await usb_resources(k)
        await result(k, "install", "ESP_ERR_INVALID_STATE")
        await result(k, "bad_phy", "ESP_ERR_INVALID_ARG")
        await cable(w, "unplug", "no_cable")
        await console_line(k, rf"USB detached 1 rhport={port}", 60)
        await w.send_str(json.dumps({"usb": {"cmd": "plug"}}))
        await keyboard(w)
        await console_line(k, rf"USB attached 2 rhport={port} speed=full configuration=1", 60)
        await typed(k, w)

        await result(k, "mask")
        await cable(w, "reset", "waiting")
        await result(k, "unmask")
        await keyboard(w)
        await typed(k, w)

        await result(k, "disconnect")
        await next_json(w, lambda j: j.get("usb", {}).get("stage") == "waiting")
        await result(k, "connect")
        await keyboard(w)
        await typed(k, w)

        # A deferred reset/SETUP must disappear at teardown, including when
        # no PHY exists (ESP-TinyUSB's skip_setup branch below).
        await result(k, "mask")
        await k.send(kd.USB_BUS, bytes([4]))
        await k.send(kd.USB_SETUP, bytes([0x80, 6, 0, 1, 0, 0, 18, 0]))
        await result(k, "uninstall")
        await next_json(w, lambda j: j.get("usb", {}).get("stage") == "waiting")
        await result(k, "uninstall", "ESP_ERR_INVALID_STATE")
        await k.send(kd.USB_SETUP, bytes([0x80, 6, 0, 1, 0, 0, 18, 0]))
        await k.send(kd.USB_OUT, bytes([0, 0]))
        await k.send(kd.USB_BUS, bytes([2]))
        await k.send(kd.USB_BUS, bytes([3]))
        await result(k, "unmask")
        await result(k, "install")
        await keyboard(w)
        await typed(k, w)

        if not direct:
            await result(k, "uninstall")
            await next_json(w, lambda j: j.get("usb", {}).get("stage") == "waiting")
            await result(k, "invalid", "ESP_ERR_INVALID_ARG")
            await result(k, "bad_task", "ESP_ERR_INVALID_ARG")
            await result(k, "high_speed", "ESP_ERR_NOT_SUPPORTED")
            await result(k, "high_speed_skip_phy", "ESP_ERR_NOT_SUPPORTED")
            await result(k, "bad_descriptors", "ESP_ERR_TIMEOUT", timeout=180)
            lines = await result(k, "race", timeout=180)
            raced = [line.rsplit(" ", 1)[-1] for line in lines if line.startswith("USB race ")]
            assert sorted(raced) == ["ESP_ERR_INVALID_STATE", "ESP_OK"], lines
            await keyboard(w)
            await typed(k, w)
            await result(k, "uninstall")
            await next_json(w, lambda j: j.get("usb", {}).get("stage") == "waiting")
            await result(k, "skip_phy")
            await keyboard(w)
            await result(k, "install", "ESP_ERR_INVALID_STATE")
            await typed(k, w)
            await result(k, "uninstall")
            await next_json(w, lambda j: j.get("usb", {}).get("stage") == "waiting")
            await result(k, "install")
            await keyboard(w)

        if direct:
            # Force a blocked real dcd_event_handler: the application's own
            # task is stopped with a full queue before tusb_deinit(). The
            # emulator must cancel pending IRQ delivery without hanging or
            # sending into the queue after the stack frees it.
            depth = queue_depth(fw)
            await result(k, "pause_consumer")
            for _ in range(depth + 2):
                await k.send(kd.USB_SETUP, bytes([0x80, 6, 0, 1, 0, 0, 18, 0]))
                await result(k, "yield")
            await result(k, "uninstall")
            await next_json(w, lambda j: j.get("usb", {}).get("stage") == "waiting")
            await result(k, "install")
            await keyboard(w)
            await typed(k, w)

        # Exercise the delivery/teardown overlap repeatedly with a live
        # consumer, without masking interrupts explicitly first.
        for _ in range(3):
            await result(k, "report")
            await result(k, "uninstall")
            await next_json(w, lambda j: j.get("usb", {}).get("stage") == "waiting")
            await result(k, "install")
            await keyboard(w)
        after = await usb_resources(k)
        assert after[0] >= before[0] and after[1] == before[1], (before, after)
        no_panic(k)
    finally:
        if w is not None:
            await w.close()
            await w.session.close()
        await k.stop()


@pytest.fixture(scope="module", params=["keyboard", "mouse", "separate", "shared", "msc"])
def usb_hid_firmware(request, exe, kode_sdk):
    """Build each real descriptor architecture through the phase-1 SDK route."""
    source = CACHE / "usb-fixtures" / f"hid_{request.param}"
    if source.exists():
        shutil.rmtree(source)
    shutil.copytree(FIXTURE, source)
    sdkconfig = source / "sdkconfig.defaults"
    configured = sdkconfig.read_text()
    hid_count = "CONFIG_TINYUSB_HID_COUNT="
    entries = [line for line in configured.splitlines() if line.startswith(hid_count)]
    assert len(entries) == 1, "the HID interface count must have a unique definition"
    configured = configured.replace(entries[0], f"{hid_count}{2 if request.param == 'separate' else 1}")
    if request.param == "msc":
        setting = "CONFIG_TINYUSB_MSC_ENABLED=n"
        assert configured.count(setting) == 1
        configured = configured.replace(setting, "CONFIG_TINYUSB_MSC_ENABLED=y")
    sdkconfig.write_text(configured + f"\nCONFIG_USB_TEST_{request.param.upper()}=y\n")
    fw = firmware(exe, source, sdk=kode_sdk)
    verify_linkage(fw, False)
    component_versions(fw)
    return fw, request.param


async def hid_enumerated(w, mode):
    has_keyboard, has_mouse = mode != "mouse", mode != "keyboard"
    stage = "hid" if has_keyboard and has_mouse else "keyboard" if has_keyboard else "mouse"
    if mode == "msc":
        stage = "mounted"
    u = (await next_json(w, lambda j: j.get("usb", {}).get("stage") == stage, timeout=120))["usb"]
    assert u["cable"] and not u["error"]
    assert u["hid"]["keyboard"] == has_keyboard and u["hid"]["mouse"] == has_mouse
    assert len(u["interfaces"]) == (2 if mode in ("separate", "msc") else 1)
    assert all(not i["error"] for i in u["interfaces"])
    assert all(i["hid_protocol"] == "report" for i in u["interfaces"] if i["class"] == 3)
    if mode in ("separate", "shared", "msc"):
        assert u["device"]["class"] == "composite"
    if mode in ("shared", "msc"):
        ids = [r["report_id"] for r in u["interfaces"][0]["reports"]]
        assert len(ids) == 2 and len(set(ids)) == 2 and all(ids)
    return u


async def usb_state(w, predicate):
    return (await next_json(w, lambda j: "usb" in j and predicate(j["usb"]), timeout=120))["usb"]


def mouse_event(state):
    return next((r["mouse"] for r in state["hid"]["reports"] if r["mouse"] is not None), None)


async def real_hid_input(k, w, mode):
    keyboard, mouse = mode != "mouse", mode != "keyboard"
    await w.send_str(json.dumps({"usb": {"cmd": "clear_typed"}}))
    await usb_state(w, lambda u: u["typed"] == "")
    if keyboard:
        await hid_report(k, "key 2 4")
    if mouse:
        await hid_report(k, "mouse 5 12 -7 1")
    u = await usb_state(w, lambda u: (not keyboard or u["typed"] == "A" and u["keys_down"] == [4])
                        and (not mouse or u["mouse"]["buttons"] == [1, 3]))
    assert u["modifiers"] == (2 if keyboard else 0)
    if mouse:
        m = mouse_event(u)
        assert m["relative"] == {"x": 12, "y": -7, "wheel": 1}
        assert m["absolute"] == {}
        assert m["total"] == {"x": 12, "y": -7, "wheel": 1}
    if keyboard:
        await hid_report(k, "key 2 4")                 # a held key is not typed twice
    if mouse:
        await hid_report(k, "mouse 5 12 -7 1")         # identical deltas still accumulate
        u = await usb_state(w, lambda u: mouse_event(u) is not None and mouse_event(u)["total"].get("x") == 24)
        assert mouse_event(u)["total"] == {"x": 24, "y": -14, "wheel": 2}
        assert mouse_event(u)["sequence"] == 2
    if keyboard:
        assert u["typed"] == "A" and u["keys_down"] == [4]
    if mode == "msc":
        storage = await console_line(k, r"USB storage sector=\d+", 30)
        assert u["disk"]["block_size"] == int(storage.rsplit("=", 1)[1])
    return u


async def test_real_hid_descriptors_reports_and_lifecycle(exe, usb_hid_firmware, tmp_path):
    fw, mode = usb_hid_firmware
    k = await boot(exe, tmp_path, fw)
    w = late = None
    try:
        await console_line(k, r"USB result install ESP_OK", 120)
        await console_line(k, r"USB versions idf=v6\.1(?:[ .-]|$)", 30)
        w = await ws(k.sim)
        await w.send_str(json.dumps({"usb": {"cmd": "plug"}}))
        await hid_enumerated(w, mode)
        u = await real_hid_input(k, w, mode)
        before = await usb_resources(k)
        late = await ws(k.sim)
        snapshot = await usb_state(late, lambda _: True)
        for field in ("interfaces", "hid", "keys_down", "modifiers", "mouse", "typed"):
            assert snapshot[field] == u[field]
        # Explicit releases cannot affect the other collection's state.
        if mode != "mouse":
            await hid_report(k, "key 0 0")
            released = await usb_state(w, lambda u: u["keys_down"] == [] and u["modifiers"] == 0)
            assert released["mouse"]["buttons"] == ([1, 3] if mode != "keyboard" else [])
        if mode != "keyboard":
            await hid_report(k, "mouse 0 0 0 0")
            await usb_state(w, lambda u: u["mouse"]["buttons"] == [])
        # Hold fresh state across each lifecycle boundary: every new
        # attachment starts with empty held keys/buttons and motion totals.
        for boundary in ("reset", "cable", "disconnect", "uninstall", "reboot"):
            if mode != "mouse":
                await hid_report(k, "key 0 5")
            if mode != "keyboard":
                await hid_report(k, "mouse 2 -3 4 -1")
            await usb_state(w, lambda u: (mode == "mouse" or u["keys_down"] == [5])
                            and (mode == "keyboard" or u["mouse"]["buttons"] == [2]))
            if boundary == "reset":
                clearing = await cable(w, "reset", "waiting")
            elif boundary == "cable":
                clearing = await cable(w, "unplug", "no_cable")
            elif boundary == "reboot":
                await k.send_json({"power": "reset"})
                clearing = await usb_state(w, lambda u: u["stage"] == "waiting")
            else:
                await result(k, boundary)
                clearing = await usb_state(w, lambda u: u["stage"] == "waiting")
            cleared = await usb_state(late, lambda u: u["log"] == clearing["log"] and u["stage"] == clearing["stage"])
            assert cleared["hid"]["reports"] == []
            assert cleared["keys_down"] == [] and cleared["modifiers"] == 0 and cleared["mouse"]["buttons"] == []
            if boundary == "cable":
                await w.send_str(json.dumps({"usb": {"cmd": "plug"}}))
            elif boundary == "disconnect":
                await result(k, "connect")
            elif boundary == "uninstall":
                await result(k, "install")
            u = await hid_enumerated(w, mode)
            assert u["hid"]["reports"] == []
            after = await usb_resources(k)
            assert after[0] >= before[0] and after[1] == before[1], (boundary, before, after)
        no_panic(k)
    finally:
        for socket in (w, late):
            if socket is not None:
                await socket.close()
                await socket.session.close()
        await k.stop()


def hid_source(state, kind):
    source = next(r for r in state["hid"]["reports"] if r["mouse"] is not None) if kind == "mouse" else next(
        r for r in state["hid"]["reports"] if r["keys_down"] is not None)
    interfaces = [i for i in state["interfaces"] if i["active"] and i["class"] == 3]
    instance = next(n for n, i in enumerate(interfaces) if i["number"] == source["interface"])
    interface = interfaces[instance]
    layout = next(r for r in interface["reports"] if r["report_id"] == source["report_id"])
    return instance, interface, layout, source


def assert_hid_unchanged(actual, previous):
    for field in ("hid", "keys_down", "modifiers", "mouse", "typed"):
        assert actual[field] == previous[field], field


def assert_hid_page(states):
    process = subprocess.run(["node", str(Path(__file__).with_name("hid_page.mjs"))],
                             input=json.dumps(states), capture_output=True, text=True, timeout=30)
    assert process.returncode == 0, process.stdout + process.stderr


async def test_real_hid_layouts_invalid_reports_and_endpoint_isolation(exe, usb_hid_firmware, tmp_path):
    fw, mode = usb_hid_firmware
    keyboard, mouse = mode != "mouse", mode != "keyboard"
    k = await boot(exe, tmp_path, fw)
    w = None
    try:
        await console_line(k, r"USB result install ESP_OK", 120)
        w = await ws(k.sim)
        await w.send_str(json.dumps({"usb": {"cmd": "plug"}}))
        await hid_enumerated(w, mode)
        valid = await real_hid_input(k, w, mode)
        assert_hid_page([valid])
        if mode == "msc":
            name = "roundtrip.bin"
            data = bytes(range(256)) * 4
            await w.send_str(json.dumps({"usb": {"cmd": "put", "name": name,
                                                "data": base64.b64encode(data).decode()}}))
            copied = await usb_state(w, lambda u: any(f["name"] == name and f["size"] == len(data)
                                                    for f in u.get("disk", {}).get("files", [])))
            assert_hid_unchanged(copied, valid)
            await w.send_str(json.dumps({"usb": {"cmd": "get", "name": name}}))
            read = await usb_state(w, lambda u: f'crc32 {zlib.crc32(data):08x}' in u["last"])
            assert f"{len(data)} bytes" in read["last"]
            assert read["read"] > copied["read"] and read["written"] > 0
            assert_hid_unchanged(read, valid)
            assert_hid_page([read])
        kind = "keyboard" if keyboard else "mouse"
        instance, interface, layout, source = hid_source(valid, kind)
        report_id = layout["report_id"]
        prefix = int(report_id is not None)
        for wire_length in (layout["length"] - 1, layout["length"] + 1):
            payload = bytes(wire_length - prefix)
            command = f"raw {instance} {report_id or 0} " + payload.hex(" ")
            await hid_report(k, command.rstrip())
            invalid = await usb_state(w, lambda u: any(
                i["number"] == interface["number"] and f"received {len(payload)}" in i["error"]
                for i in u["interfaces"]))
            assert_hid_unchanged(invalid, valid)
        if report_id is not None:
            declared = {r["report_id"] for r in interface["reports"]}
            unknown = next(i for i in range(1, 256) if i not in declared)
            await hid_report(k, f"raw {instance} {unknown}")
            invalid = await usb_state(w, lambda u: any(
                f"unknown HID input Report ID {unknown}" in i["error"] for i in u["interfaces"]))
            assert_hid_unchanged(invalid, valid)
        if keyboard:
            await hid_report(k, "key 0 1")
            invalid = await usb_state(w, lambda u: any("rollover" in i["error"] for i in u["interfaces"]))
            assert_hid_unchanged(invalid, valid)
            await hid_report(k, "key 0 0")
        if mouse:
            await hid_report(k, "mouse 0 0 0 0")
        await usb_state(w, lambda u: not u["keys_down"] and not u["mouse"]["buttons"]
                        and all(not i["error"] for i in u["interfaces"]))

        if keyboard:
            for bit in range(8):
                modifier = 1 << bit
                await hid_report(k, f"key {modifier} 0")
                await usb_state(w, lambda u: u["modifiers"] == modifier and not u["keys_down"])
            await w.send_str(json.dumps({"usb": {"cmd": "clear_typed"}}))
            await usb_state(w, lambda u: not u["typed"])
            await hid_report(k, "key 0 4 5 6")
            await usb_state(w, lambda u: u["keys_down"] == [4, 5, 6] and u["typed"] == "abc")
            await hid_report(k, "key 0 4 5 6")
            await hid_report(k, "key 0 5 6")
            await usb_state(w, lambda u: u["keys_down"] == [5, 6] and u["typed"] == "abc")
            await hid_report(k, "key 0 0")
            await usb_state(w, lambda u: not u["keys_down"])
            for layout_name, text in (("us", ";"), ("es", "ñ")):
                await w.send_str(json.dumps({"usb": {"cmd": "layout", "layout": layout_name}}))
                await usb_state(w, lambda u: u["layout"] == layout_name)
                await w.send_str(json.dumps({"usb": {"cmd": "clear_typed"}}))
                await usb_state(w, lambda u: not u["typed"])
                await hid_report(k, "key 0 51")
                await usb_state(w, lambda u: u["typed"] == text and u["keys_down"] == [51])
                await hid_report(k, "key 0 0")
                await usb_state(w, lambda u: not u["keys_down"])

        if mouse:
            u = await usb_state_after_command(w, {"cmd": "layout", "layout": "es"})
            _, interface, _, _ = hid_source(u, "mouse")
            buttons = interface["buttons"]
            mask = sum(1 << (b - 1) for b in buttons)
            await hid_report(k, f"mouse {mask} 0 0 0")
            await usb_state(w, lambda u: u["mouse"]["buttons"] == buttons)
            await hid_report(k, "mouse 0 0 0 0")
            u = await usb_state(w, lambda u: not u["mouse"]["buttons"])
            before = mouse_event(u)
            button = buttons[0]
            await hid_report(k, f"mouse {1 << (button - 1)} 0 0 0")
            await hid_report(k, "mouse 0 0 0 0")
            u = await usb_state(w, lambda u: not u["mouse"]["buttons"] and mouse_event(u)["releases"].get(str(button), 0)
                                == before["releases"].get(str(button), 0) + 1)
            assert mouse_event(u)["presses"][str(button)] == before["presses"].get(str(button), 0) + 1
            before = mouse_event(u)
            before_motion = u
            for _ in range(2):
                await hid_report(k, "mouse 0 127 -127 1")
            valid = await usb_state(w, lambda u: mouse_event(u)["sequence"] == before["sequence"] + 2)
            assert mouse_event(valid)["total"] == {
                "x": before["total"]["x"] + 254, "y": before["total"]["y"] - 254,
                "wheel": before["total"]["wheel"] + 2}
            assert_hid_page([before_motion, valid])
            await hid_report(k, "mouse 0 -128 0 0")
            invalid = await usb_state(w, lambda u: any("outside logical range" in i["error"] for i in u["interfaces"]))
            assert_hid_unchanged(invalid, valid)
            await hid_report(k, "mouse 0 0 0 0")
            await usb_state(w, lambda u: mouse_event(u)["sequence"] == mouse_event(valid)["sequence"] + 1
                            and all(not i["error"] for i in u["interfaces"]))

        current = await usb_state_after_command(w, {"cmd": "layout", "layout": "es"})
        await w.close()
        await w.session.close()
        w = await ws(k.sim)
        reconnected = await usb_state(w, lambda _: True)
        assert_hid_unchanged(reconnected, current)
        assert_hid_page([current, reconnected])

        await w.send_str(json.dumps({"usb": {"cmd": "clear_typed"}}))
        await usb_state(w, lambda u: not u["typed"])
        if keyboard:
            await hid_report(k, "key 0 52")
        if mouse:
            await hid_report(k, "mouse 1 1 2 3")
        held = await usb_state(w, lambda u: (not keyboard or u["keys_down"] == [52])
                               and (not mouse or u["mouse"]["buttons"] == [1]))
        kind = "mouse" if mouse else "keyboard"
        instance, _, _, source = hid_source(held, kind)
        endpoint = source["endpoint"]
        await result(k, f"endpoint {instance} 0")
        closed = await usb_state(w, lambda u: all(r["endpoint"] != endpoint for r in u["hid"]["reports"]))
        assert closed["hid"]["reports"] == [r for r in held["hid"]["reports"] if r["endpoint"] != endpoint]
        await hid_report(k, "mouse 1 1 2 3" if mouse else "key 0 8", "ESP_FAIL")
        unchanged = await usb_state_after_command(w, {"cmd": "layout", "layout": "es"})
        assert_hid_unchanged(unchanged, closed)
        await result(k, f"endpoint {instance} 1")
        if mouse:
            await hid_report(k, "mouse 1 1 2 3")
            reopened = await usb_state(w, lambda u: mouse_event(u) is not None)
            assert mouse_event(reopened)["sequence"] == 1
            assert mouse_event(reopened)["total"] == {"x": 1, "y": 2, "wheel": 3}
        if keyboard:
            await hid_report(k, "key 0 8")
            await usb_state(w, lambda u: u["typed"] == ("é" if mode == "separate" else "e"))
        no_panic(k)
    finally:
        if w is not None:
            await w.close()
            await w.session.close()
        await k.stop()
    assert k.sim.proc.returncode == 0, k.sim.stderr()


async def usb_state_after_command(w, command):
    assert command["cmd"] == "layout"
    other = "us" if command["layout"] == "es" else "es"
    await w.send_str(json.dumps({"usb": {"cmd": "layout", "layout": other}}))
    await usb_state(w, lambda u: u["layout"] == other)
    await w.send_str(json.dumps({"usb": command}))
    return await usb_state(w, lambda u: u["layout"] == command["layout"])


async def test_real_hid_repeated_simulator_shutdown_and_startup(exe, usb_hid_firmware, tmp_path):
    fw, mode = usb_hid_firmware
    for cycle in range(3):
        directory = tmp_path / str(cycle)
        directory.mkdir()
        k = await boot(exe, directory, fw)
        w = None
        try:
            await console_line(k, r"USB result install ESP_OK", 120)
            w = await ws(k.sim)
            await w.send_str(json.dumps({"usb": {"cmd": "plug"}}))
            state = await hid_enumerated(w, mode)
            assert not state["hid"]["reports"]
            await real_hid_input(k, w, mode)
            no_panic(k)
        finally:
            if w is not None:
                await w.close()
                await w.session.close()
            await k.stop()
        assert k.sim.proc.returncode == 0, k.sim.stderr()

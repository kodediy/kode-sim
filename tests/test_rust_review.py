"""`kode-emu review`, from outside: the binary and the fake esp-emu.

The checks, verdict.json, console.log, the screenshots and the exit codes
0, 1 and 2. The buttons and the taps go to the board's chips in-process;
the fake reads the expander as kodeOS does.
"""

import json
import os
import struct
import subprocess
import zlib

import pytest

APP = b"\xe9" + bytes(200) + b"kemu_rx" + bytes(range(256)) * 20     # an app built for the simulator


@pytest.fixture
def setup(fake_emu, tmp_path):
    kodeos = tmp_path / "kodeos"
    kodeos.mkdir()
    (kodeos / "merged-binary.bin").write_bytes(b"\xe9kodeOS" + bytes(1000))
    (kodeos / "kodeos.elf").write_bytes(b"elf")
    app = tmp_path / "app.bin"
    app.write_bytes(APP)
    return tmp_path, kodeos, app


def run(exe, tmp, kodeos, app, scenario):
    s = tmp / "scenario.json"
    s.write_text(json.dumps(scenario))
    out = tmp / "out"
    p = subprocess.run([str(exe), "review", str(app), str(s), "--out", str(out), "--kodeos", str(kodeos)],
                       env={**os.environ, "KODE_EMU_CACHE": str(tmp / "cache")},
                       capture_output=True, text=True, timeout=120)
    verdict = json.loads((out / "verdict.json").read_text()) if (out / "verdict.json").exists() else None
    return p.returncode, verdict, out, p.stderr


def checks(verdict):
    return {c["name"]: c["ok"] for c in verdict["checks"]}


def test_an_app_that_does_what_the_scenario_asks_passes(exe, setup):
    tmp, kodeos, app = setup
    rc, v, out, err = run(exe, tmp, kodeos, app, {"settle_s": 0.5, "steps": [
        {"wait": 0.2},
        {"button": "right", "hold_ms": 100},
        {"tap": [10, 20]},
        {"console": "hola"},
        {"expect_console": "echo: hola", "timeout_s": 5},
        {"screenshot": "after"},
    ]})
    assert rc == 0, (v, err)
    assert v["ok"] is True
    assert checks(v) == {"kodeos_boots": True, "installs": True, "app_starts": True,
                         "expect_console: echo: hola": True, "home_by_hold": True, "no_crash": True}
    assert list(v) == ["ok", "checks", "screenshots", "log", "duration_s"]
    assert v["screenshots"] == ["kodeos.png", "app.png", "after.png", "home.png"]
    assert v["log"] == "console.log" and isinstance(v["duration_s"], float)
    for name in v["screenshots"] + ["console.log"]:
        assert (out / name).exists(), name
    log = (out / "console.log").read_text()
    assert "Project name:     app" in log
    stamped = (out / "console.log").read_bytes().decode().split("\n")[:-1]     # a line may hold a \r
    assert all(line[9] == " " and float(line[:9]) >= 0 for line in stamped), log[:200]
    assert "ok   installs" in err and "kode-emu review: passed in " in err


def test_a_screenshot_is_the_screen_as_a_png(exe, setup):
    """The fake paints a 2x2 white square at (0, 0) on a dark screen."""
    tmp, kodeos, app = setup
    rc, v, out, _ = run(exe, tmp, kodeos, app, {"settle_s": 0.2, "home_hold": False})
    assert rc == 0, v
    png = (out / "app.png").read_bytes()
    assert png[:8] == b"\x89PNG\r\n\x1a\n"
    assert struct.unpack(">IIBBBBB", png[16:29]) == (410, 502, 8, 2, 0, 0, 0)
    at = png.index(b"IDAT")
    rows = zlib.decompress(png[at + 4:at + 4 + struct.unpack(">I", png[at - 4:at])[0]])
    stride = 1 + 410 * 3
    assert rows[1:7] == b"\xff" * 6 and rows[7:10] == b"\x00" * 3
    assert rows[stride + 1:stride + 7] == b"\xff" * 6 and rows[2 * stride + 1:2 * stride + 4] == b"\x00" * 3
    assert "home_by_hold" not in checks(v)


def test_an_app_that_crashes_fails(exe, setup):
    tmp, kodeos, app = setup
    rc, v, _, err = run(exe, tmp, kodeos, app, {"settle_s": 0.2, "steps": [{"console": "crash"}, {"wait": 1}]})
    assert rc == 1
    assert checks(v)["no_crash"] is False
    assert "Guru Meditation" in next(c["detail"] for c in v["checks"] if c["name"] == "no_crash")
    assert "FAIL no_crash: " in err and "kode-emu review: failed in " in err


def test_what_the_console_never_says_fails_the_review(exe, setup):
    tmp, kodeos, app = setup
    rc, v, out, _ = run(exe, tmp, kodeos, app, {"settle_s": 0.2, "steps": [
        {"expect_console": "never said", "timeout_s": 1}]})
    assert rc == 1
    assert checks(v)["expect_console: never said"] is False
    assert next(c["detail"] for c in v["checks"] if c["name"] == "expect_console: never said") == \
        "not said within 1s"
    assert "failed.png" in v["screenshots"] and (out / "failed.png").exists()


def test_what_the_console_must_not_say_fails_the_review(exe, setup):
    tmp, kodeos, app = setup
    rc, v, _, _ = run(exe, tmp, kodeos, app, {"settle_s": 0.2, "home_hold": False, "forbid": ["echo: b[a-z]d"],
                                              "steps": [{"console": "bad"}, {"wait": 0.3}]})
    assert rc == 1
    assert checks(v)["forbid: echo: b[a-z]d"] is False
    assert next(c["detail"] for c in v["checks"] if c["name"].startswith("forbid")) == "echo: bad"


def test_a_scenario_that_takes_too_long_fails_in_time(exe, setup):
    tmp, kodeos, app = setup
    rc, v, _, _ = run(exe, tmp, kodeos, app, {"timeout_s": 4, "settle_s": 0.2, "steps": [{"wait": 30}]})
    assert rc == 1
    assert checks(v)["in_time"] is False
    assert next(c["detail"] for c in v["checks"] if c["name"] == "in_time") == "the scenario took more than 4s"


def test_what_is_not_an_app_for_the_simulator_is_not_reviewed(exe, setup):
    tmp, kodeos, app = setup
    app.write_bytes(b"not an app at all")
    rc, v, _, err = run(exe, tmp, kodeos, app, {})
    assert (rc, v) == (2, None) and "not an ESP-IDF app image" in err
    app.write_bytes(b"\xe9" + bytes(4000))                  # an ESP-IDF image, but built for the board
    rc, _, _, err = run(exe, tmp, kodeos, app, {})
    assert rc == 2 and "built for the board" in err


def test_a_scenario_it_does_not_understand_is_not_played(exe, setup):
    tmp, kodeos, app = setup
    assert run(exe, tmp, kodeos, app, {"steps": [{"dance": True}]})[0] == 2
    assert run(exe, tmp, kodeos, app, {"steps": [{"button": "middle"}]})[0] == 2
    assert run(exe, tmp, kodeos, app, {"timeout": 5})[0] == 2


def test_a_scenario_with_a_bad_pattern_or_wait_is_not_played(exe, setup):
    tmp, kodeos, app = setup
    assert run(exe, tmp, kodeos, app, {"steps": [{"expect_console": "(unclosed"}]})[0] == 2
    assert run(exe, tmp, kodeos, app, {"steps": [{"wait": "a while"}]})[0] == 2


def test_a_review_that_cannot_run_exits_2(exe, setup):
    tmp, kodeos, app = setup
    s = tmp / "bad.json"
    s.write_text("{not json")
    p = subprocess.run([str(exe), "review", str(app), str(s), "--out", str(tmp / "out"), "--kodeos", str(kodeos)],
                       capture_output=True, text=True, timeout=60)
    assert p.returncode == 2 and "bad.json" in p.stderr
    blocked = tmp / "file"
    blocked.write_text("")
    p = subprocess.run([str(exe), "review", str(app), "--out", str(blocked / "out"), "--kodeos", str(kodeos)],
                       capture_output=True, text=True, timeout=60)
    assert p.returncode == 2 and "cannot write" in p.stderr, p.stderr


def test_the_kodeos_folder_comes_from_the_environment(exe, setup):
    tmp, kodeos, app = setup
    p = subprocess.run([str(exe), "review", str(app), "--out", str(tmp / "out")],
                       env={**os.environ, "KODE_EMU_KODEOS": str(kodeos)},
                       capture_output=True, text=True, timeout=120)
    assert p.returncode == 0, p.stderr
    p = subprocess.run([str(exe), "review", str(app)], env={k: v for k, v in os.environ.items()
                                                             if k != "KODE_EMU_KODEOS"},
                       capture_output=True, text=True, timeout=60)
    assert p.returncode == 2 and "--kodeos" in p.stderr


def run_in(exe, tmp, kodeos, app, scenario):
    """run(), with a TMPDIR of the test's own: what the review leaves there is seen."""
    t = tmp / "t"
    t.mkdir(exist_ok=True)
    s = tmp / "scenario.json"
    s.write_text(json.dumps(scenario))
    p = subprocess.run([str(exe), "review", str(app), str(s), "--out", str(tmp / "out"), "--kodeos", str(kodeos)],
                       env={**os.environ, "KODE_EMU_CACHE": str(tmp / "cache"), "TMPDIR": str(t)},
                       capture_output=True, text=True, timeout=120)
    return p, t


def test_a_time_no_clock_can_count_is_not_played(exe, setup):
    tmp, kodeos, app = setup
    for scenario in [{"settle_s": 1e300}, {"timeout_s": 1e300}, {"steps": [{"wait": 1e300}]},
                     {"steps": [{"wait": -1}]}, {"steps": [{"button": "top", "hold_ms": 1e300}]},
                     {"steps": [{"expect_console": "x", "timeout_s": 1e300}]}]:
        p, t = run_in(exe, tmp, kodeos, app, scenario)
        assert p.returncode == 2 and "from 0 to" in p.stderr, (scenario, p.returncode, p.stderr)
        assert list(t.glob("kode-emu-review-*")) == [], scenario


def test_a_screenshot_goes_nowhere_but_the_out_folder(exe, setup):
    tmp, kodeos, app = setup
    for name in ["../evil", str(tmp / "evil"), "a/b", ".."]:
        p, _ = run_in(exe, tmp, kodeos, app, {"settle_s": 0.2, "steps": [{"screenshot": name}]})
        assert p.returncode == 2 and "a screenshot's name" in p.stderr, (name, p.stderr)
    assert not (tmp / "evil.png").exists()


def test_a_pattern_is_python_s(exe, setup):
    """Braces that quantify nothing are braces, and lookarounds work, as in Python's re."""
    tmp, kodeos, app = setup
    rc, v, _, err = run(exe, tmp, kodeos, app, {"settle_s": 0.2, "home_hold": False, "forbid": [r"(?<=echo: )bad"],
                                                "steps": [{"console": '{"ok":true}'},
                                                          {"expect_console": '{"ok":true}', "timeout_s": 5},
                                                          {"console": "temp=21"},
                                                          {"expect_console": r"(?<=temp=)\d+", "timeout_s": 5}]})
    assert rc == 0, (v, err)
    assert checks(v)['expect_console: {"ok":true}'] is True


def test_a_review_stopped_by_a_signal_leaves_nothing_behind(exe, setup, fake_emu):
    import signal
    import time
    tmp, kodeos, app = setup
    t = tmp / "t"
    t.mkdir()
    s = tmp / "scenario.json"
    s.write_text(json.dumps({"settle_s": 0.2, "steps": [{"wait": 60}]}))
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        p = subprocess.Popen([str(exe), "review", str(app), str(s), "--out", str(tmp / "out"), "--kodeos", str(kodeos)],
                             env={**os.environ, "KODE_EMU_CACHE": str(tmp / "cache"), "TMPDIR": str(t)},
                             stderr=subprocess.PIPE, text=True)
        try:
            end = time.time() + 30
            while not list(t.glob("kode-emu-review-*/flash.bin")) or not fake_emu.exists():
                assert time.time() < end and p.poll() is None, "the review never started"
                time.sleep(0.1)
            time.sleep(1.5)                                  # booted, installing or playing
            p.send_signal(sig)
            rc = p.wait(timeout=15)
        finally:
            if p.poll() is None:
                p.kill()
        err = p.stderr.read()
        assert rc == 128 + sig, (sig, rc, err)
        assert "stopped by" in err, err
        assert list(t.glob("kode-emu-review-*")) == [], (sig, list(t.iterdir()))
        left = subprocess.run(["pgrep", "-f", str(t)], capture_output=True, text=True)
        assert left.stdout.strip() == "", f"esp-emu left running: {left.stdout}"

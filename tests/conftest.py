import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
CACHE = ROOT / ".cache"
HERE = Path(__file__).resolve().parent


@pytest.fixture(scope="session")
def kode_sdk() -> Path:
    """The newest kode-sdk: $KODE_SDK as it is, or a clone kept in .cache/
    and brought to the tip of kode-sdk's default branch (develop) at every
    session (offline, it stays as it was, and says so)."""
    if os.environ.get("KODE_SDK"):
        return Path(os.environ["KODE_SDK"])
    dest = CACHE / "kode-sdk"
    if not dest.exists():
        CACHE.mkdir(exist_ok=True)
        subprocess.run(["git", "clone", "-q", "--depth", "1",
                        "https://github.com/kodediy/kode-sdk.git", str(dest)], check=True)
        return dest
    fetched = subprocess.run(["git", "-C", str(dest), "fetch", "-q", "--depth", "1", "origin", "HEAD"],
                             capture_output=True, text=True)
    if fetched.returncode == 0:
        subprocess.run(["git", "-C", str(dest), "reset", "-q", "--hard", "FETCH_HEAD"], check=True)
    else:
        print(f"kode-sdk: could not fetch its default branch, testing {dest} as it is: {fetched.stderr.strip()}")
    return dest


def _cargo() -> str:
    for c in (shutil.which("cargo"), "/opt/homebrew/opt/rustup/bin/cargo", Path.home() / ".cargo" / "bin" / "cargo"):
        if c and Path(c).exists():
            return str(c)
    pytest.skip("no cargo to build kode-emu with")


@pytest.fixture(scope="session")
def exe() -> Path:
    """The Rust kode-emu, built now if its sources moved on."""
    from sim import EXE
    cargo = _cargo()
    env = {**os.environ, "PATH": f"{Path(cargo).parent}{os.pathsep}{os.environ.get('PATH', '')}"}   # its rustc
    subprocess.run([cargo, "build", "--release", "-q", "-p", "kode-emu"], cwd=ROOT, env=env, check=True)
    return EXE


@pytest.fixture
def fake_emu(tmp_path, monkeypatch):
    """tests/fake_esp_emu.py standing in for esp-emu; each launch's
    arguments are a line in the file this returns."""
    exe = tmp_path / "esp-emu"
    exe.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{HERE / "fake_esp_emu.py"}" "$@"\n')
    exe.chmod(0o755)
    log = tmp_path / "launches"
    monkeypatch.setenv("KODE_EMU_ESP_EMU", str(exe))
    monkeypatch.setenv("FAKE_LAUNCH_LOG", str(log))
    # The C5's image: the fake boots anything, so two empty files will do.
    c5 = tmp_path / "c5"
    c5.mkdir()
    for f in ("merged-binary.bin", "kode_c5.elf"):
        (c5 / f).write_bytes(b"")
    monkeypatch.setenv("KODE_EMU_C5", str(c5))
    return log


@pytest.fixture
async def sim(exe, fake_emu, tmp_path):
    """`kode-emu run` on the fake esp-emu, with a blank 8 MB card. Its
    teardown fails if it went away before the test ended, or did not end
    with 0 on SIGTERM (Sim.stop_clean)."""
    from sim import start
    s = await start(exe, tmp_path)
    yield s
    s.stop_clean()

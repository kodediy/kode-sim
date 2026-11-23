#!/bin/bash
# kode-sim for Linux (x86_64 or arm64; Debian, Ubuntu, Fedora or Arch): install
# everything, then start kodeOS in the simulator and open the Kode Dot in Chrome.
#
#   curl -fsSL https://sim.kode.diy/install-linux.sh | bash
#
# Run it again any time: it skips what is already there, updates kode-sim and
# kodeOS, and starts the simulator. Under WSL (install-windows.ps1) the page
# opens in Windows' own browser.
set -e

KODE_HOME="${KODE_HOME:-$HOME/kode}"
IDF_VERSION="v6.1"
IDF="$HOME/.espressif/v6.1/esp-idf"
PORT="${KODE_SIM_PORT:-8765}"
URL="http://127.0.0.1:$PORT/"

say() { printf '\n\033[1;33m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31m%s\033[0m\n' "$*"; exit 1; }

[ "$(uname -s)" = "Linux" ] || fail "This is the Linux script. Use install-mac.sh or install-windows.ps1."
case "$(uname -m)" in x86_64|aarch64) ;; *) fail "kode-sim needs an x86_64 or arm64 computer." ;; esac
IS_WSL=0
grep -qi microsoft /proc/version 2>/dev/null && IS_WSL=1
SUDO=""
[ "$(id -u)" = 0 ] || SUDO="sudo"

say "System packages"
if command -v apt-get >/dev/null; then
    $SUDO apt-get update -qq
    $SUDO apt-get install -y -qq git curl wget rsync mtools flex bison gperf python3 python3-pip python3-venv \
        cmake ninja-build ccache libffi-dev libssl-dev dfu-util libusb-1.0-0 build-essential pkg-config xdg-utils
elif command -v dnf >/dev/null; then
    $SUDO dnf install -y git curl wget rsync mtools flex bison gperf python3 python3-pip cmake ninja-build ccache \
        libffi-devel openssl-devel dfu-util libusbx gcc gcc-c++ make pkgconf xdg-utils
elif command -v pacman >/dev/null; then
    $SUDO pacman -Sy --needed --noconfirm git curl wget rsync mtools flex bison gperf python python-pip cmake ninja \
        ccache libffi openssl dfu-util libusb base-devel pkgconf xdg-utils
else
    fail "No apt, dnf or pacman here: install git, rsync, mtools, cmake, ninja and python3 yourself, then run this again."
fi

if [ "$IS_WSL" = 0 ] && ! command -v google-chrome >/dev/null && ! command -v chromium >/dev/null \
        && ! command -v chromium-browser >/dev/null; then
    say "Google Chrome"
    if command -v apt-get >/dev/null && [ "$(uname -m)" = x86_64 ]; then
        wget -q -O /tmp/chrome.deb https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb
        $SUDO apt-get install -y -qq /tmp/chrome.deb && rm -f /tmp/chrome.deb
    elif command -v dnf >/dev/null && [ "$(uname -m)" = x86_64 ]; then
        $SUDO dnf install -y https://dl.google.com/linux/direct/google-chrome-stable_current_x86_64.rpm
    elif command -v apt-get >/dev/null; then
        $SUDO apt-get install -y -qq chromium || true
    elif command -v pacman >/dev/null; then
        $SUDO pacman -S --needed --noconfirm chromium || true
    fi
fi

say "Rust"
if [ ! -x "$HOME/.cargo/bin/cargo" ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
. "$HOME/.cargo/env"

say "ESP-IDF $IDF_VERSION (the first time: 10 to 20 minutes)"
if [ ! -d "$IDF" ]; then
    mkdir -p "$(dirname "$IDF")"
    git clone -b "$IDF_VERSION" --recursive --depth 1 --shallow-submodules https://github.com/espressif/esp-idf.git "$IDF"
fi
(cd "$IDF" && ./install.sh esp32p4,esp32c5 >/dev/null)

say "kode-sim"
mkdir -p "$KODE_HOME" "$HOME/.local/bin"
if [ -d "$KODE_HOME/kode-sim/.git" ]; then
    git -C "$KODE_HOME/kode-sim" pull --ff-only
else
    git clone https://github.com/kodediy/kode-sim.git "$KODE_HOME/kode-sim"
fi
(cd "$KODE_HOME/kode-sim" && cargo build --release)
ln -sf "$KODE_HOME/kode-sim/target/release/kode-sim" "$HOME/.local/bin/kode-sim"
grep -q '.local/bin' "$HOME/.bashrc" 2>/dev/null || echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$HOME/.bashrc"
# kode-sim finds ESP-IDF through IDF_PATH in every new terminal.
grep -q 'IDF_PATH=' "$HOME/.bashrc" 2>/dev/null || echo "export IDF_PATH=\"$IDF\"" >> "$HOME/.bashrc"

say "kodeOS"
if [ -d "$KODE_HOME/KodeOS/.git" ]; then
    git -C "$KODE_HOME/KodeOS" pull --ff-only
else
    git clone https://github.com/kodediy/KodeOS.git "$KODE_HOME/KodeOS"
fi
git -C "$KODE_HOME/KodeOS" submodule update --init --recursive

open_page() {
    if [ "$IS_WSL" = 1 ]; then
        # Windows' Chrome, as an app window; its default browser if Chrome is not there.
        powershell.exe -NoProfile -Command "try { Start-Process chrome -ArgumentList '--app=$URL','--window-size=520,760' -ErrorAction Stop } catch { Start-Process '$URL' }" >/dev/null 2>&1
        return
    fi
    for b in google-chrome google-chrome-stable chromium chromium-browser; do
        if command -v "$b" >/dev/null; then
            ("$b" --app="$URL" --window-size=520,760 >/dev/null 2>&1 &)
            return
        fi
    done
    xdg-open "$URL" >/dev/null 2>&1 || echo "Open $URL in your browser."
}

say "Starting the Kode Dot (the first build of kodeOS takes a while)"
. "$IDF/export.sh" >/dev/null
"$HOME/.local/bin/kode-sim" run --no-open --port "$PORT" "$KODE_HOME/KodeOS" &
SIM=$!
trap 'kill $SIM 2>/dev/null' INT TERM
until curl -s -o /dev/null "$URL"; do
    kill -0 $SIM 2>/dev/null || fail "kode-sim stopped: read its messages above."
    sleep 2
done
open_page
say "The Kode Dot is at $URL  (Ctrl-C here stops it)"
wait $SIM

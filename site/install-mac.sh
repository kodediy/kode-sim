#!/bin/bash
# kode-sim for macOS (Apple silicon): install everything, then start kodeOS
# in the simulator and open the Kode Dot in Chrome.
#
#   curl -fsSL https://sim.kode.diy/install-mac.sh | bash
#
# Run it again any time: it skips what is already there, updates kode-sim and
# kodeOS, and starts the simulator.
set -e

KODE_HOME="${KODE_HOME:-$HOME/kode}"
IDF_VERSION="v6.1"
IDF="$HOME/.espressif/v6.1/esp-idf"
PORT="${KODE_SIM_PORT:-8765}"
URL="http://127.0.0.1:$PORT/"

say() { printf '\n\033[1;33m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31m%s\033[0m\n' "$*"; exit 1; }

[ "$(uname -s)" = "Darwin" ] || fail "This is the macOS script. Use install-linux.sh or install-windows.ps1."
[ "$(uname -m)" = "arm64" ] || fail "kode-sim needs a Mac with Apple silicon (M1 or later)."

say "Homebrew"
if ! command -v brew >/dev/null 2>&1 && [ ! -x /opt/homebrew/bin/brew ]; then
    /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)" </dev/tty
fi
eval "$(/opt/homebrew/bin/brew shellenv)"
brew install git cmake ninja dfu-util python@3.12 rsync
if [ ! -d "/Applications/Google Chrome.app" ]; then
    brew install --cask google-chrome
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
grep -q '.local/bin' "$HOME/.zshrc" 2>/dev/null || echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$HOME/.zshrc"
# kode-sim finds ESP-IDF through IDF_PATH in every new terminal.
grep -q 'IDF_PATH=' "$HOME/.zshrc" 2>/dev/null || echo "export IDF_PATH=\"$IDF\"" >> "$HOME/.zshrc"

say "kodeOS"
if [ -d "$KODE_HOME/KodeOS/.git" ]; then
    git -C "$KODE_HOME/KodeOS" pull --ff-only
else
    git clone https://github.com/kodediy/KodeOS.git "$KODE_HOME/KodeOS"
fi
git -C "$KODE_HOME/KodeOS" submodule update --init --recursive

say "Starting the Kode Dot (the first build of kodeOS takes a while)"
. "$IDF/export.sh" >/dev/null
"$HOME/.local/bin/kode-sim" run --no-open --port "$PORT" "$KODE_HOME/KodeOS" &
SIM=$!
trap 'kill $SIM 2>/dev/null' INT TERM
until curl -s -o /dev/null "$URL"; do
    kill -0 $SIM 2>/dev/null || fail "kode-sim stopped: read its messages above."
    sleep 2
done
open -na "Google Chrome" --args --app="$URL" --window-size=520,760 2>/dev/null || open "$URL"
say "The Kode Dot is at $URL  (Ctrl-C here stops it)"
wait $SIM

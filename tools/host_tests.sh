#!/bin/sh
# The host suites: kode-emu's Rust (the chips, cosim, the binary's units),
# the binary from outside against the fake esp-emu, the page's sim.js in
# node (when there is one), and kemu's C side.
set -e
cd "$(dirname "$0")/.."
cargo test -q
cargo build --release -q -p kode-emu
.venv/bin/pytest -q -m "not emu" tests
if command -v node >/dev/null; then node --test tests/test_page.mjs; else echo "no node: tests/test_page.mjs not run"; fi
cmake -S firmware/tests -B build_tests >/dev/null
cmake --build build_tests >/dev/null
ctest --test-dir build_tests --output-on-failure

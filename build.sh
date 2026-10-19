#!/usr/bin/env sh
# Build do Bitu. A layer usa -Bsymbolic para evitar interposição dos símbolos
# Vulkan pelo loader (libvulkan exporta os mesmos nomes e é carregada antes).
set -e
cd "$(dirname "$0")"
export CARGO_HOME="$PWD/.cargo"
export RUSTFLAGS="-C link-arg=-Wl,-Bsymbolic"
cargo build --release -p bitu-layer "$@"
unset RUSTFLAGS
cargo build --release -p bitu-gui "$@"
cargo build --release -p bitu-inject "$@"

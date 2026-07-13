# Local `ul-build` build image: compiles and tests the gui crate (tauri)
# under WSL, where the webkit2gtk -dev packages cannot be installed on the host
# (no sudo). Reconstructed from the history of the image built by
# hand on 2026-07-10 (rust:1.89-bookworm base at the time), now versioned.
#
# Build:          docker build -t ul-build docker/ul-build/
# Verification:   docker run --rm ul-build rustc --version
# Typical use:    docker run --rm -v "$PWD":/work -w /work ul-build \
#                    cargo test -p universallink-gui --locked
#
# The Rust version must stay aligned with the development toolchain and
# with the CI one (.github/workflows/ci.yml, "Rust toolchain" step).
FROM rust:1.97-bookworm

# Same webview -dev packages as the CI's "System webview dependencies (Linux)"
# step: tauri-runtime links them even without launching the webview engine.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       libwebkit2gtk-4.1-dev libgtk-3-dev \
    && rm -rf /var/lib/apt/lists/*

# clippy and rustfmt: the image is also used to lint/format the gui crate.
RUN rustup component add clippy rustfmt

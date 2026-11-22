# SPDX-License-Identifier: GPL-3.0-or-later
# Debian 13 (trixie) with libpurple 2.14 development files, Finch and
# BitlBee for the libpurple 2 plugin's build and smoke test, and Rust
# from rustup (latest stable, as mimid).
FROM docker.io/library/debian:trixie
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      build-essential cmake pkg-config git ca-certificates curl \
      libpurple-dev libglib2.0-dev python3 \
      finch bitlbee \
 && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain stable
ENV PATH=/root/.cargo/bin:$PATH

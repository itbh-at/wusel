# Linux test environment for wusel (FUSE needs a Linux kernel).
# Deliberately using mise as the toolchain manager, so that the Rust version
# comes from the same mise.toml as on the macOS host — one source of truth.

FROM debian:trixie-slim

# UTF-8 locale so an interactive `ls` shows Unicode file names (e.g. "für.txt")
# instead of escaping them; C.UTF-8 is built into glibc, no locales package needed.
ENV LANG=C.UTF-8

RUN apt-get update && apt-get install -y --no-install-recommends \
        curl ca-certificates git \
        build-essential pkg-config \
        fuse3 libfuse3-dev \
    && rm -rf /var/lib/apt/lists/*

# /etc/fuse.conf stays stock — no `user_allow_other`: wusel deliberately never
# mounts with allow_other (see wusel-fuse's fs.rs — a personal cloud mount must
# not be readable by every local user), and a stock fuse.conf makes the dev
# container surface any regression of that property instead of masking it.

# Install mise (same role as on the host). Pinned via MISE_VERSION: mise pins
# every other tool, so the installer must not be the one floating piece. Keep
# in sync with the host (`mise version`).
RUN curl -fsSL https://mise.run | MISE_VERSION=v2026.6.10 sh
ENV PATH="/root/.local/bin:/root/.local/share/mise/shims:${PATH}"
ENV MISE_TRUSTED_CONFIG_PATHS="/work"
# Login shells (bash -lc) should also find mise — they otherwise reset PATH.
RUN printf 'export PATH="/root/.local/bin:/root/.local/share/mise/shims:$PATH"\n' \
        > /etc/profile.d/mise.sh

WORKDIR /work

# Copy only mise.toml and pre-install the toolchain → layer caching.
COPY mise.toml /work/mise.toml
RUN mise trust /work/mise.toml && mise install

# Linux builds go into their own target directory, separate from the macOS target.
ENV CARGO_TARGET_DIR="/work/target-linux"

CMD ["bash"]

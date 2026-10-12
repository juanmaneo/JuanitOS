# Stage 0 of the JuanitOS bootstrap: a reproducible Linux toolchain that
# builds every Rust artifact (host workspace, UEFI applications, firewall
# image) on any machine with Docker. It does NOT build the ISO: that needs
# FreeBSD's makefs/pkg/tar and runs in the FreeBSD builder VM
# (`jos-release builder`) or on JuanitOS itself; see docs/bootstrap.md.
#
#   docker build -t juanitos-build .
#   docker run --rm -v "$PWD:/src" -v juanitos-rustup:/usr/local/rustup juanitos-build
#   # -> target/docker/ ; add `check` to run fmt/clippy/tests instead
#
# The source is bind-mounted, nothing is baked into the image.

FROM rust:1-bookworm

# The toolchain itself (channel, targets, components) comes from the
# repository's rust-toolchain.toml; rustup installs it on first use. Only
# the system tools are baked in.
RUN apt-get update \
 && apt-get install -y --no-install-recommends mtools dosfstools \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
ENV CARGO_TARGET_DIR=/src/target/docker
# Keep the toolchain rustup downloads between runs (mount a named volume
# here; rustup needs its temp dir on the same filesystem as the toolchains).
VOLUME /usr/local/rustup
COPY tools/docker-build.sh /usr/local/bin/juanitos-build
ENTRYPOINT ["juanitos-build"]

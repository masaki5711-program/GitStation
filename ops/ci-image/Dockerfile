# The job image of GitStation's own CI (`.forge-ci.yml`, plan 14 X):
# the pinned Rust toolchain (rust-toolchain.toml) with rustfmt and clippy,
# cargo-nextest, git, and every crate of Cargo.lock already downloaded, so a
# job builds and tests with no network (job containers run with
# `--network=none`, ADR-0029). See ops/ci-image/README.md.
#
# Build context: the repository root.
#   node ops/ci-image/build-and-push.mjs        (build, push to the test registry, print the digest)
#   docker build -f ops/ci-image/Dockerfile -t gitstation/ci-rust:1.99.1 .
#
# When rust-toolchain.toml changes: change RUST_VERSION and the base digest
# together (the build fails if they disagree with the file).

ARG RUST_VERSION=1.99.1
ARG RUST_IMAGE=docker.io/library/rust:1.99.1-slim-bookworm@sha256:f2c004e3d2d8b547e8cf7ffbc29205385f4e71d23e53e4e383065db5e9f822cd

# ── cargo-nextest, built from crates.io with its own lockfile ──
FROM ${RUST_IMAGE} AS nextest
ARG NEXTEST_VERSION=0.9.150
ARG CARGO_BUILD_JOBS=4
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
RUN cargo install cargo-nextest --version "=${NEXTEST_VERSION}" --locked --root /opt/nextest \
    && /opt/nextest/bin/cargo-nextest nextest --version

# ── the workspace's manifests only, with empty sources ──
# Only Cargo.toml / Cargo.lock reach the next stage, so a source change does
# not invalidate the downloaded registry (the COPY --from below hashes only
# what this stage leaves).
FROM ${RUST_IMAGE} AS manifests
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN find crates -type f ! -name Cargo.toml -delete \
    && find crates -mindepth 1 -type d -empty -delete \
    && for m in crates/*/Cargo.toml; do \
         d="${m%/Cargo.toml}"; mkdir -p "$d/src" && : > "$d/src/lib.rs"; \
         sed -n 's/^path *= *"\(.*\.rs\)".*/\1/p' "$m" | while read -r p; do \
           mkdir -p "$d/$(dirname "$p")" && : > "$d/$p"; \
         done; \
       done

# ── the job image ──
FROM ${RUST_IMAGE} AS ci
ARG RUST_VERSION
ARG NEXTEST_VERSION=0.9.150
LABEL org.opencontainers.image.title="gitstation-ci-rust" \
      org.opencontainers.image.description="GitStation CI job image: Rust ${RUST_VERSION} (rustfmt, clippy), cargo-nextest ${NEXTEST_VERSION}, git, offline crates of Cargo.lock" \
      org.opencontainers.image.source="ops/ci-image/Dockerfile" \
      io.gitstation.rust.version="${RUST_VERSION}" \
      io.gitstation.rust.components="rustfmt,clippy" \
      io.gitstation.nextest.version="${NEXTEST_VERSION}"

# git: forge-git and the tests shell out to it (>= 2.38, ADR-0006; bookworm
# has 2.39; checked below: gc --cruft, pack.writeBitmapLookupTable).
# hostname: forge-core's instance id tests.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git hostname \
    && rm -rf /var/lib/apt/lists/* \
    && git --version \
    && git --version | awk '{ split($3, v, "."); if (v[1] < 2 || (v[1] == 2 && v[2] < 38)) { print "git >= 2.38 required, found " $3; exit 1 } }'

# The toolchain of rust-toolchain.toml, with its components, as the default:
# rustup must never need the network inside a job.
RUN rustup set profile minimal \
    && rustup component add --toolchain "${RUST_VERSION}" rustfmt clippy \
    && rustup default "${RUST_VERSION}"

COPY --from=nextest /opt/nextest/bin/cargo-nextest /usr/local/cargo/bin/cargo-nextest

# Every crate of Cargo.lock (all targets and platforms), downloaded and
# unpacked into CARGO_HOME. The rust-toolchain.toml check makes the image
# fail to build when it no longer matches the workspace.
COPY --from=manifests /src /opt/ci/manifests
RUN cd /opt/ci/manifests \
    && grep -qx "channel = \"${RUST_VERSION}\"" rust-toolchain.toml \
    && rustc --version | grep -q "^rustc ${RUST_VERSION} " \
    && cargo fetch --locked \
    && cargo metadata --locked --offline --format-version 1 > /dev/null \
    && chmod -R a+rX "${CARGO_HOME}" "${RUSTUP_HOME}"
# registry/cache (the .crate files) must stay: an offline build with only
# registry/src fails with "failed to download ... --offline was specified".

# Jobs have no network (`--network=none`); cargo must not try.
ENV CARGO_NET_OFFLINE=true \
    CARGO_TERM_COLOR=always \
    CARGO_INCREMENTAL=0 \
    RUST_BACKTRACE=1

# The job runs as root *inside* the container, on purpose: forge-runner
# passes no --user and makes the work directory 0755 owned by the runner's
# own user, which rootless Podman maps to container root. A non-root USER
# here cannot write /workspace (tested 2026-11-11: "touch: Permission
# denied"). Container root is the unprivileged runner user on the host, with
# --cap-drop=all, no-new-privileges, a read-only root and no network.
# `ci` (uid 1000) is ready for when the runner maps the work directory to it
# (`--userns=keep-id:uid=1000,gid=1000`; README "非 root で動かすには").
RUN useradd --uid 1000 --user-group --create-home --shell /bin/bash ci
USER root
WORKDIR /workspace

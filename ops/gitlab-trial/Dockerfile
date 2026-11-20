# GitStation (forge) runtime image for the PFS trial server, built ON that
# server by the GitLab job `build:image` (.gitlab-ci.yml; the company
# registry is not used). See ops/gitlab-trial/README.md.
#
# Build context: the repository root (.dockerignore keeps target/, .git,
# web/node_modules and web/dist out).
#   docker build -f ops/gitlab-trial/Dockerfile \
#     --build-arg FORGE_UID="$(id -u)" --build-arg FORGE_GID="$(id -g)" \
#     -t gitstation/forge:<short sha> .
#
# FORGE_UID / FORGE_GID: the runner user's ids (gitstation-ci). The
# container user owns the bind-mounted data and reads the 0600 secret files
# of /srv/gitstation with them, so nothing on the host is chowned to a
# container uid.

ARG RUST_IMAGE=docker.io/library/rust:1.99.1-slim-bookworm@sha256:f2c004e3d2d8b547e8cf7ffbc29205385f4e71d23e53e4e383065db5e9f822cd
# Pinned by digest (taken from the local images 2026-11-20); change tag and
# digest together.
ARG NODE_IMAGE=docker.io/library/node:22.23.1-bookworm-slim@sha256:6c74791e557ce11fc957704f6d4fe134a7bc8d6f5ca4403205b2966bd488f6b3
ARG RUNTIME_IMAGE=docker.io/library/debian:bookworm-slim@sha256:60eac759739651111db372c07be67863818726f754804b8707c90979bda511df

# ── 1. the web UI (web/dist, embedded into the binary by `embed-ui`) ──
FROM ${NODE_IMAGE} AS web
WORKDIR /src/web
# An internal npm mirror, when the server has no route to the public one.
ARG NPM_CONFIG_REGISTRY
ENV NPM_CONFIG_REGISTRY=${NPM_CONFIG_REGISTRY}
COPY web/package.json web/package-lock.json ./
RUN --mount=type=cache,id=gitstation-trial-npm,target=/root/.npm npm ci --no-audit --no-fund
COPY web/ ./
RUN nice -n 10 npm run build && test -f dist/index.html

# ── 2. forge (release, --features embed-ui; as ops/linux/build-release.sh) ──
FROM ${RUST_IMAGE} AS forge
# 3 of the shared server's 8 CPUs, under nice (the server is shared).
ARG CARGO_BUILD_JOBS=3
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} \
    CARGO_INCREMENTAL=0 \
    CARGO_TERM_COLOR=never
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
# Crates include files from docs/ (API documents, examples) and ops/db/.
COPY docs ./docs
COPY ops/db ./ops/db
COPY --from=web /src/web/dist ./web/dist
RUN --mount=type=cache,id=gitstation-trial-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=gitstation-trial-cargo-target,target=/src/target \
    grep -q "^channel = \"$(rustc --version | cut -d' ' -f2)\"" rust-toolchain.toml \
    && nice -n 10 cargo build --release --locked -p forge-cli --features embed-ui \
    && install -D -m 0755 target/release/forge /out/forge

# ── 3. runtime ──
FROM ${RUNTIME_IMAGE} AS runtime
ARG FORGE_UID=10001
ARG FORGE_GID=10001
# git: forge executes git (>= 2.38, ADR-0006; bookworm has 2.39).
# curl: the compose health check of /readyz. openssh-client: forge-deploy
# (the worker runs `ssh` to the deploy targets, ADR-0031; [deploy] in
# forge.toml); bookworm's package, fixed by the digest of RUNTIME_IMAGE
# like the others. tini: PID 1 that forwards SIGTERM to forge and reaps git
# children.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates curl openssh-client tini \
    && rm -rf /var/lib/apt/lists/* \
    && ssh -V \
    && git --version | awk '{ split($3, v, "."); if (v[1] < 2 || (v[1] == 2 && v[2] < 38)) { print "git >= 2.38 required, found " $3; exit 1 } }' \
    && groupadd --gid "${FORGE_GID}" forge \
    && useradd --uid "${FORGE_UID}" --gid "${FORGE_GID}" --create-home --home-dir /home/forge --shell /usr/sbin/nologin forge \
    && install -d -o forge -g forge -m 0750 /var/lib/forge /var/lib/forge/repositories
COPY --from=forge /out/forge /usr/local/bin/forge
# The bootstrap SQL travels with the binary that needs it (deploy.sh reads
# it from the checkout; kept here for a manual re-run).
COPY ops/db/bootstrap /usr/local/share/forge/db/bootstrap
ARG GIT_COMMIT=unknown
LABEL org.opencontainers.image.title="gitstation-forge" \
      org.opencontainers.image.description="GitStation forge (serve / worker / db migrate) for the PFS trial server" \
      org.opencontainers.image.source="ops/gitlab-trial/Dockerfile" \
      org.opencontainers.image.revision="${GIT_COMMIT}"
ENV FORGE_CONFIG=/etc/forge/forge.toml
USER forge
WORKDIR /home/forge
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/forge"]
CMD ["serve"]

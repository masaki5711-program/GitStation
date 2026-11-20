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
ARG NODE_IMAGE=docker.io/library/node:22.23.1-bookworm-slim
ARG RUNTIME_IMAGE=docker.io/library/debian:bookworm-slim

# ── 1. the web UI (web/dist, embedded into the binary by `embed-ui`) ──
FROM ${NODE_IMAGE} AS web
WORKDIR /src/web
# An internal npm mirror, when the server has no route to the public one.
ARG NPM_CONFIG_REGISTRY
ENV NPM_CONFIG_REGISTRY=${NPM_CONFIG_REGISTRY}
COPY web/package.json web/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build && test -f dist/index.html

# ── 2. forge (release, --features embed-ui; as ops/linux/build-release.sh) ──
FROM ${RUST_IMAGE} AS forge
ARG CARGO_BUILD_JOBS=4
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
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    grep -q "^channel = \"$(rustc --version | cut -d' ' -f2)\"" rust-toolchain.toml \
    && cargo build --release --locked -p forge-cli --features embed-ui \
    && install -D -m 0755 target/release/forge /out/forge

# ── 3. runtime ──
FROM ${RUNTIME_IMAGE} AS runtime
ARG FORGE_UID=10001
ARG FORGE_GID=10001
# git: forge executes git (>= 2.38, ADR-0006; bookworm has 2.39).
# curl: the compose health check of /readyz. tini: PID 1 that forwards
# SIGTERM to forge and reaps git children.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates curl tini \
    && rm -rf /var/lib/apt/lists/* \
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

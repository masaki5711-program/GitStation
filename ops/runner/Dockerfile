# forge-runner in a container, for the test environment on a machine that has
# Docker but no Linux host to spare (ops/runner/README.md). Production runners
# stay on their own Linux host (runbook ci-runner, plan 14 §10.1).
#
# Build context: the repository root.
#   docker build -f ops/runner/Dockerfile -t gitstation/forge-runner:test .
#
# The runner runs as the unprivileged user `podman` and starts each job with
# rootless Podman inside this container. No Docker socket is mounted.

FROM rust:1.99.1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
# forge-client embeds docs/api/agent-tools.toml.
COPY docs/api ./docs/api
RUN cargo build --release --locked -p forge-runner

FROM quay.io/podman/stable:v5.3@sha256:c7c150ebacb82f778c7f5c8d8eec56358575736c5451e00f68e1057de709c4a6
RUN dnf -y install git-core && dnf clean all \
    && mkdir -p /etc/forge-runner /var/lib/forge-runner \
    && chown podman:podman /var/lib/forge-runner
COPY --from=build /src/target/release/forge-runner /usr/local/bin/forge-runner
USER podman
ENTRYPOINT ["/usr/local/bin/forge-runner", "--config", "/etc/forge-runner/config.toml"]
CMD ["run"]

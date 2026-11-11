# The PostgreSQL service of GitStation's own CI (`.forge-ci.yml`, job `test`):
# the test registry's postgres:16 with max_connections=200 on port 55432, as
# ops/dev/compose.yml and .github/workflows/ci.yml run it (a job service has
# no `command` key, so the setting lives in the image). Built and pushed by
# `node ops/ci-image/build-and-push.mjs` inside the test runner's Podman.
FROM registry:5000/ci/postgres@sha256:2addf63b441276adf4ffc426a70a9a63d642f633b5c84affee13d54a52056abc
LABEL org.opencontainers.image.title="gitstation-ci-postgres" \
      org.opencontainers.image.description="postgres:16 with max_connections=200 on port 55432 for GitStation CI"
CMD ["postgres", "-c", "max_connections=200", "-c", "port=55432"]

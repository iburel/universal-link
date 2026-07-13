# Control-plane deployment image (`universallink-server`).
#
# The server listens IN CLEARTEXT: TLS is terminated by a reverse proxy (see
# `deploy/`). Never expose it directly to the Internet.
#
# Two stages:
#   1. build   — rust:1.97-bookworm (buildpack-deps base: gcc + perl already
#      present, which is enough for `ring`, rustls' crypto provider; neither cmake
#      nor NASM, since we do not use aws-lc-rs). Version aligned with the CI
#      toolchain (.github/workflows/ci.yml) and with the `ul-build` image.
#   2. runtime — debian:bookworm-slim: SAME glibc as the build stage (bookworm),
#      so the dynamically linked binary runs as is. The TLS roots are
#      compiled INTO the binary (webpki-roots, via reqwest/rustls): no need
#      for the `ca-certificates` package to reach the OIDC issuer over HTTPS.
#
# Build (the context MUST be the repository root):
#   docker build -t universallink-server -f docker/server/Dockerfile .
# In practice, go through deploy/docker-compose.yml instead.

FROM rust:1.97-bookworm AS build
WORKDIR /src
# The context is the workspace root: cargo needs all the members' manifests,
# but only COMPILES the requested package and its subtree. Neither the GUI
# (webkit) nor the Core (iroh) are pulled in — the server does not depend on them.
COPY . .
RUN cargo build --release --locked -p universallink-server-daemon --bin universallink-server

FROM debian:bookworm-slim AS runtime
# A control plane has no reason to run as root.
RUN useradd --system --uid 10001 --home-dir /data --create-home uluser
COPY --from=build /src/target/release/universallink-server /usr/local/bin/universallink-server

# Container defaults: the directory lives in /data (volume), the server listens
# on all interfaces OF THE CONTAINER'S INTERNAL NETWORK. The operator only has
# to supply the OIDC issuer and client_id (see deploy/.env.example).
ENV UNIVERSALLINK_SERVER_BIND=0.0.0.0:8080 \
    UNIVERSALLINK_SERVER_STATE=/data/universallink-directory.json
VOLUME ["/data"]
EXPOSE 8080

USER uluser
WORKDIR /data
# The binary handles SIGTERM: `docker stop` makes it exit cleanly (code 0).
ENTRYPOINT ["/usr/local/bin/universallink-server"]

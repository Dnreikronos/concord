# Multi-stage build for the Concord API server. Stage one compiles a static
# release binary against musl; stage two ships only that binary on a minimal
# Alpine base. The GUI client (concord-client / gpui) is never built here.
#
# Plain Dockerfile syntax (no BuildKit-only features) so it builds with either
# the legacy builder or BuildKit.

##### Stage 1: builder #####
FROM rust:1.95-alpine AS builder

# `ring` (the rustls crypto backend) compiles C and assembly at build time, so
# it needs a C toolchain, make, and perl. These live only in the builder stage.
RUN apk add --no-cache build-base perl

WORKDIR /build

# Copy the whole workspace and compile only the server binary in release mode.
# Building with `-p concord-server` leaves the GUI client (gpui) out of the
# image entirely. The musl host target links statically, then we strip symbols.
COPY . .
RUN cargo build --release --locked -p concord-server --bin concord-server \
    && strip target/release/concord-server

##### Stage 2: runtime #####
FROM alpine:3.20 AS runtime

# ca-certificates: TLS roots for outbound OAuth / HTTP calls.
# A dedicated unprivileged user runs the process.
RUN apk add --no-cache ca-certificates \
    && adduser -D -H -u 10001 concord

COPY --from=builder /build/target/release/concord-server /usr/local/bin/concord-server

USER concord

# HOST/PORT default to 0.0.0.0:8080 in the server config.
EXPOSE 8080

# busybox wget (bundled with Alpine) GETs the in-app /health endpoint; a non-2xx
# or refused connection yields a non-zero exit and marks the container unhealthy.
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD wget -q -O /dev/null http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["concord-server"]

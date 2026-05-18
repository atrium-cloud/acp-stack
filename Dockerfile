# syntax=docker/dockerfile:1

FROM rust:1.88.0-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY data ./data
COPY migrations ./migrations

RUN cargo build --locked --release

FROM debian:bookworm-slim AS runtime

RUN addgroup --system --gid 1000 acp \
    && adduser --system --uid 1000 --ingroup acp --home /home/acp --shell /usr/sbin/nologin acp \
    && apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /workspace \
    /workspace/uploads \
    /home/acp/.config/acp-stack \
    /home/acp/.local/share/acp-stack \
    && chown -R acp:acp /workspace /home/acp/.config /home/acp/.local/share

COPY --from=builder /app/target/release/acps /usr/local/bin/acps
COPY --from=builder /app/target/release/acpctl /usr/local/bin/acpctl

EXPOSE 7700
WORKDIR /workspace
USER acp
ENV HOME=/home/acp

CMD ["acps", "serve", "--bind", "0.0.0.0:7700"]

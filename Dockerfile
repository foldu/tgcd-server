FROM rust:1.41-slim-stretch
RUN rustup target add x86_64-unknown-linux-musl
WORKDIR /app
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM alpine:3.11.3
COPY --from=0 /app/target/x86_64-unknown-linux-musl/release/tgcd-server /
CMD ["tgcd-server"]

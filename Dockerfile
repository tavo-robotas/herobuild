from rust:1.63-alpine as builder

WORKDIR /herobuild
COPY Cargo.lock Cargo.toml .
COPY src/ src

RUN apk --no-cache add zlib zlib-dev openssl openssl-dev musl-dev

#RUN RUSTFLAGS="-Ctarget-feature=+crt-static" cargo install --path .
RUN CFLAGS=-mno-outline-atomics cargo install --path .

FROM alpine:latest
COPY --from=builder /usr/local/cargo/bin/herobuild /usr/local/bin/herobuild

RUN apk --no-cache add libc6-compat

CMD ["herobuild"]

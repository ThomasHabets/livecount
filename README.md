# livecount

Livecount is a widget for web pages showing how many browsers are
currently open to this exact page.

## Building

Normally: `cargo build --release`.

Completely static: `cargo build --target x86_64-unknown-linux-musl --release`

## Listening

There are two modes to run in.

### Mode 1: Listen on a TCP port and run TLS

`livecount --listen '[::1]:8000' --cert cert.pem --key key.pem`

### Mode 2: Listen on a unix datagram socket for fd passing

This is for use with [sni-router from
tarweb](https://github.com/ThomasHabets/tarweb).

Socket handoff mode listens on a Unix datagram socket and expects each datagram
to include one `SCM_RIGHTS` file descriptor plus the initial plain HTTP bytes to
prepend to reads from that descriptor:

`livecount --unix-listen /run/livecount.sock --unix-listen-group www-data --unix-listen-mode 660`

`--unix-listen-mode` is parsed as octal, matching normal `chmod` input.

## URLs

### /livecount/health

Test page. Not really a health page.

### /livecount/metrics

Prometheus metrics.

## TODO

* Either upgrade to warp 0.4, or throw it out and only use hyper.
* Clean up all the logic. This was literally my first Rust program, so it's a
  bit shit.

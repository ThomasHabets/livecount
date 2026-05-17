# livecount

Livecount is a widget for web pages showing how many browsers are
currently open to this exact page.

## Building

Normally: `cargo build --release`.

Completely static: `cargo build --target x86_64-unknown-linux-musl --release`

## Listening

Socket handoff mode listens on a Unix socket and expects each accepted control
connection to send one `SCM_RIGHTS` file descriptor plus the initial plain HTTP
bytes to prepend to reads from that descriptor:

`livecount --unix-listen /run/livecount.sock`

The existing direct TCP listener is still available, but must now be enabled
explicitly. It remains TLS:

`livecount --listen '[::1]:8000' --cert cert.pem --key key.pem`

## URLs

### /livecount/health

Test page. Not really a health page.

### /livecount/metrics

Prometheus metrics.

## TODO

* Either upgrade to warp 0.4, or throw it out and only use hyper.
* Clean up all the logic. This was literally my first Rust program, so it's a
  bit shit.

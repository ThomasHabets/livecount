# livecount

Livecount is a widget for web pages showing how many browsers are
currently open to this exact page.

## Building

Normally: `cargo build --release`.

Completely static: `cargo build --target x86_64-unknown-linux-musl --release`

## URLs

### /livecount/health

Test page. Not really a health page.

### /livecount/metrics

Prometheus metrics.

## TODO

* Either upgrade to warp 0.4, or throw it out and only use hyper.
* Clean up all the logic. This was literally my first Rust program, so it's a
  bit shit.

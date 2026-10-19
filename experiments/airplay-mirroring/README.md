# Host-only mirroring policy experiment

This is an isolated, picture-only proof of concept for the future CatPlay mirroring bridge. It does not start an AirPlay listener or change the adapter. Production CarPlay is unchanged.

Run from the CatPlay repository root with Rust installed:

```sh
cargo test --manifest-path experiments/airplay-mirroring/Cargo.toml --locked
cargo run --manifest-path experiments/airplay-mirroring/Cargo.toml --locked -- --enable-mirroring
```

The crate has no external dependencies and its own Cargo workspace. Without the explicit flag the executable prints help. The library defaults to refusing mirroring ownership.

The core requires exclusive source ownership and carries both a controller identity and session generation in each lease. It rejects old or foreign configuration, frame, drain and release calls. It checks H.264 descriptor metadata against caller-supplied display limits, bounds retained configuration and queued compressed bytes, and requires an IDR after configuration, overflow or discontinuity. It scans all NALs for IDR, including after AUD/SEI, and refuses in-band parameter sets. It refuses mirroring audio and remote controls.

The descriptor's coded dimensions, cropping, reference structure and decoder compatibility must be validated by a future adapter. This code does not fully parse SPS/PPS/slices or prove that the car can decode them. Timestamps remain opaque values. Queue limits cover this policy's payloads only, not upstream network buffers, allocator overhead or downstream retained packets. The sample 800 x 480 display and buffer sizes are synthetic test values, not measurements of the Peugeot or Mini Ultra1.

See [architecture and required integration work](../../docs/airplay-mirroring/architecture.md) and [build and validation evidence](../../docs/airplay-mirroring/build-and-validation.md).

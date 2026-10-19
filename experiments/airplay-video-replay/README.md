# Production video framing replay

This host harness imports the original CatPlay source files for NAL parsing, AVCC/HVCC configuration and Annex B conversion. It contains no copied replacement converter and does not modify production source.

From the CatPlay repository root:

```sh
cargo test --manifest-path experiments/airplay-video-replay/Cargo.toml --locked
cargo run --manifest-path experiments/airplay-video-replay/Cargo.toml --locked
```

The 17 imported tests run alongside five framing regressions and three tests combining the new ownership/admission policy with the real converters. The combined tests preserve SPS/PPS, AUD/SEI-leading IDR, delta NALs, and opaque timestamps, and reject a stale teardown/configuration. These are hand-authored framing fixtures, not decoder-valid video or a captured phone session.

The generic converter tests cover one-, two- and four-byte lengths. The current production screen transmitter only supports four-byte lengths; the proposed mirroring contract and policy/converter bridge tests use four. Config serialization and payload preservation do not prove that any particular H.264 stream is accepted by the car.

Not exercised: FairPlay negotiation, AES/ChaCha transport, RTSP, clock synchronization, video decoding, audio, USB, Wi-Fi, the MIPS target or the actual vehicle. The full protocol crate has separate Linux dependencies and is not replaced by this harness.

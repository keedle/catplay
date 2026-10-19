# Experimental receiver probe

This continuation adds a real AirPlay receiver entry point for protocol observation on Linux. It is gated by the disabled-by-default `experimental_airplay_mirroring` Cargo feature and an explicit runtime flag. No production gadget selects it. The probe has not been run on the Mini Ultra1 or with the iPhone Air.

## Build and validate

On a Linux host with Rust 1.99.1, a C/C++ toolchain, pkg-config, OpenSSL development files and libopus development files:

```sh
cargo test --locked -p catplay_carplay --lib --tests
cargo test --locked -p catplay_carplay --features experimental_airplay_mirroring --lib --tests
cargo build --locked -p catplay_carplay --features experimental_airplay_mirroring --example airplay_mirror_probe
cargo test --locked -p catplay_carplay --features experimental_airplay_mirroring --example airplay_mirror_probe
```

A Linux x86_64 build is not an adapter binary. The Mini Ultra1 requires the matching MIPS32r2 little-endian musl toolchain/sysroot, compatible floating-point ABI and correct timing implementation. The repository's generic musl test Dockerfile explicitly warns that its timer fails on the physical device. Do not deploy its output merely because it links.

## Receiver behavior

The picture-only profile advertises H.264 screen reception without HEVC, audio, rotation or CarPlay control capabilities. It keeps Bonjour and `/info` consistent after sink customization. It rejects unsupported media encryption, initial SETUP containing streams, non-NTP timing, audio or mixed stream SETUP, HEVC config and non-four-byte NAL lengths. These restrictions describe the implemented subset; they are not an assertion that iOS 27 will choose it.

The probe uses TCP port 7001 and a distinct random discovery name. Pairing storage remains in memory. It installs no raw logger or session trace writer and never saves mirrored media. It prints at most 128 metadata reports plus startup/summary lines. Pairing handler completion means the handler returned, not that all peer authentication has succeeded; HomeKit can carry a refusal in an HTTP 200 TLV response.

Observed metadata includes handler stages, the initial SETUP shape, timing protocol category, selected encryption type number, static refusal categories, reported video dimensions, SPS profile/level header bytes and counts of configs, frames, IDRs and compressed bytes. It does not fully parse SPS semantics or prove the picture can decode. It does not forward video, audio, HID or other commands to a car.

The new profile limits screen wire-body size to 1 MiB including any authentication tag, before allocating the requested body. Existing profiles keep their 4 MiB limit. This is an input packet bound, not a total process-memory budget; crypto prefetch, config copies, network buffers and the runtime also consume memory.

The finite duration defaults to 30 seconds and cannot exceed 120 seconds. Ctrl-C also requests cleanup. Accepted session resources, discovery and listener ownership are closed before exit, with a three-second shutdown deadline. The executable has no install, service restart, persistent configuration or firmware operation.

## Later supervised observation

The user has requested a temporary adapter test. It still requires a target-compatible artifact and a concrete cleanup procedure before execution; no such run has occurred. This request does not include firmware flashing, persistent installation or service changes. A future Linux-host observation also requires deliberately selecting the interface and connecting the test phone to the same reachable network.

The invocation shape is:

```text
airplay_mirror_probe --enable-mirroring --interface IFACE --width WIDTH --height HEIGHT --seconds 30
```

WIDTH and HEIGHT must be explicit experiment parameters, not an invented claim about the Peugeot display. Start with generated content, retain metadata only, and distinguish receiver negotiation from car playback. Stopping the probe leaves the installed service and firmware untouched, but any interrupted phone session may need reconnection.

## Shared fixes and regressions

These changes also fix shared lifecycle and keyframe behavior used by normal CarPlay. Session admission now precedes sink initialization; failed initialization cleans up before releasing the lock. The C2A adapter emits a disconnect once for an initialized owner and never for a rejected uninitialized sink. Video keyframes are detected across the scanned NALs, including metadata before an IDR, with invalid AVC mixed-slice/header cases refused.

Tests include real localhost session competition and rollback, source-import tests of the actual C2A adapter, and AES screen receive to compressed `EncodedVideoFrame` to independent ChaCha output. The encrypted test checks fragmentation, exact compressed bytes, destination-clock serialization with controlled clocks and authentication-tag rejection. It does not test pairing with Apple, a live clock synchronization exchange, pixel decoding or a car.

The first Linux baseline passed 76 of 77 library tests. The existing drift test expected an active PLL although production deliberately sets `DISABLE_PLL=true`. Its replacement explicitly tests the configured offset-only behavior in both directions; production timing logic was not changed to make that test pass.

## Continuation validation record

The [successful hosted Linux run](https://github.com/keedle/catplay/actions/runs/39280228558) validated revision `6af0cb0dfcfdae6e6dcb81420c98cc3711dd9ed3` on Ubuntu 24.04 with Rust 1.99.1. The default configuration passed 89 library and eight integration tests. The experimental configuration passed 95 library and eight integration tests; its receiver probe built and passed two CLI tests. Both configurations passed the 43 portable tests and both synthetic smoke replays. Integration coverage includes real receiver admission/rollback, C2A disconnect lifecycle, encrypted screen conversion and authentication-tag refusal.

The application code is identical in later revision `886bb2ad9b930c14e9a31e54c3671033e7934001`. The intervening changes affect only the runner image and documentation. Earlier notes incorrectly treated a redundant queued retry as the only current evidence; the archived successful run is authoritative for this application code. These results do not validate MIPS execution, live Apple pairing, a decoder or car playback.

The [MIPS build](mips-probe-build-plan.md) and physical phone/car tests remain outstanding.

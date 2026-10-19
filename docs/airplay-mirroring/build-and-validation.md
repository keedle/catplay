# AirPlay mirroring build and validation

Continuation note: this document retains the earlier audit/build history. See
[integrated mode](integrated-mode.md) for the current screen/audio implementation
and validation scope. Earlier statements about pending integration are historical.

This guide records a reproducible validation path for the AirPlay mirroring experiments. Default CatPlay startup remains CarPlay. A feature-enabled build now has an explicit experimental process mode connecting the picture-only receiver to the car-facing transmitter; see [integrated mode](integrated-mode.md). Full application CI and physical validation of this continuation remain pending. Treat the standalone experiment and replay harness as research tools until the protocol and session blockers are resolved.

## Source pins and evidence levels

The source baseline for this work is:

| Source | Revision | Role |
| --- | --- | --- |
| `work/catplay` | `bc6edae49554f2d0160b18ed18fc7b7376353115` | CatPlay Rust workspace and device app |
| `work/catplay-firmware` | `aca307bb24efc7b1bcf624625e609a6666970761` | Yocto layers, recipes, board configuration |

The firmware checkout records its dependencies as git submodules. At this baseline their gitlink pins are BitBake `acfe02fa38b5da9e6a36c6cedcf91d4fcbefbfbd`, meta-clang `ef0d22f21910a0c30c967eeb17d8f209806e2d81`, meta-freescale `2245584775b04e393c6d36daa19037d17e295352`, meta-lts-mixins-rust `00c30d00f7377ee426b017bb46dd8c4c7eeea03b`, meta-openembedded `14282a02be9c74a1276a7cda7d6c89e054699a11`, meta-rust `4751b33ae8aa03a901941454461391dcf80c2988`, and openembedded-core `5d1aa5c806c061a2994f4decb59016610f093213`. Build against these recorded pins. A missing or uninitialized submodule means the checkout is not yet a build tree.

Report results with an evidence label:

- **Static/source evidence:** a claim follows from checked-in code or recipe configuration.
- **Host test:** named command passed on the host; this does not establish MIPS behavior.
- **Cross-build:** named target and toolchain linked the artifact; this does not establish execution on the adapter.
- **Generated fixture replay:** deterministic synthetic bytes exercised a parser/harness; this does not establish behavior with captured traffic or a live phone.
- **Captured trace replay:** a parser/harness consumed a documented physical capture; this does not itself establish a live end-to-end session or rendering path.
- **Physical trace:** bytes, logs, and observations were captured from the adapter/phone at a named stage. State the exact device identity, firmware revision, phone/iOS version, connection path, and what was actually observed.
- **Manual device result:** a person ran a reversible temporary build on the device. This is distinct from any synthetic replay or compile result.

Never promote an inference, generated fixture replay, or captured trace replay to an end-to-end physical test claim. Record failed and partial runs alongside their commands and environment.

## Host checks on macOS arm64

Use the isolated stable Rust toolchain under `work/toolchain`; keep it out of the shell's global configuration. From the CatPlay checkout root:

```sh
export PATH="$PWD/../toolchain/cargo/bin:$PATH"
export RUSTUP_HOME="$PWD/../toolchain/rustup"
export CARGO_HOME="$PWD/../toolchain/cargo"
rustc --version
cargo --version
cargo test -p catplay_carplay --lib --locked
cargo test -p catplay_iap2_link --lib --locked
```

Run from `work/catplay`, so the workspace manifest and `Cargo.lock` control dependency selection. The full `cargo test -p catplay_carplay --lib --locked` was attempted on macOS and fails to compile `catplay_tracing` because `lowprio_queue.rs` uses unavailable `SYS_gettid` and a `pthread_setname_np` signature that does not match macOS. This portability issue is recorded and will not be fixed in this effort. The full protocol test suite did not run on macOS because its build failed. A later Linux validation workflow on the personal fork compiles the actual library; see receiver-probe.md for continuation evidence. A passing test does not mean the whole firmware app builds.

Both experiments are directories committed in the feature branch. Each has a nested Cargo workspace, so invoke it by manifest path from the `work/catplay` root and keep `--locked` to honor that workspace's lockfile:

```sh
cargo test --manifest-path experiments/airplay-mirroring/Cargo.toml --locked
cargo test --manifest-path experiments/airplay-video-replay/Cargo.toml --locked
cargo run --manifest-path experiments/airplay-video-replay/Cargo.toml --locked
cargo run --manifest-path experiments/airplay-mirroring/Cargo.toml --locked -- --enable-mirroring
```

The replay harness passes 20 imported production-source tests, including three new random-access detection tests, five framing tests and three combined policy/converter tests. These tests cover video framing/conversion paths; they do not exercise crypto, RTSP negotiation, or a decoder. The standalone policy passes 15 unit tests. Both synthetic replay executables pass. The full protocol suite cannot run on this Mac; its Linux continuation results are recorded below. Label the replay inputs accurately: deterministic generated fixtures are **generated fixture replay**; bytes from an actual device session are **captured trace replay**. Neither label means a full live session succeeded.

## Integrated application checks

The Linux workflow also defines an application matrix for normal features and
`experimental_airplay_mirroring`. Each job builds the actual `catplay_c2a` binary
and runs its library tests without starting the application or accessing USB,
Bluetooth, a phone or an adapter. This covers application integration that the
protocol-only jobs do not compile.

The jobs use Ubuntu 22.04 and Rust 1.99.1. The application requires the TurboJPEG
3 API, so CI builds libjpeg-turbo 3.0.1 in a temporary local prefix and verifies
its source archive SHA-256 before use. This host dependency does not alter the
firmware's pinned compiler or target libraries.

The workflow definition is not a passing result. Record the completed run and
revision before treating an integrated application change as Linux-validated.
A Linux build remains distinct from a compatible MIPS build and from observing
an actual picture on the Peugeot screen.

## Linux/MIPS cross-build and firmware build

The supported product build path is the Yocto recipe, not an ad-hoc Rust target invocation. `meta-catplay/recipes-apps/catplay_git.bb` applies MIPS-specific relocation, BFD linker and `libatomic` flags, and depends on target libraries including multimedia, graphics, D-Bus, USB and OpenSSL. The generic musl Docker test image itself warns that its `Instant::now()` does not work on the physical target. Use the firmware layer's matching MIPS sysroot and compiler for any production app artifact.

The firmware README still names scarthgap and BitBake 5.0.10, but the pinned checkout's `.gitmodules` selects BitBake 2.18 and wrynose for OE-core, meta-clang, meta-openembedded and meta-freescale. Follow the exact gitlink revisions above, not the stale README branch names. The Rust mixin still names scarthgap/rust; its compatibility must be checked during a hosted parse preflight. The `clk-mini-ultra-nor` machine config selects `catplay` and `catplay-g-iphone`; `c2a-system-bundle.bb` gathers the image, initrd, and kernel outputs. Once the pinned submodules are populated and the documented build environment plus a valid `conf/bblayers.conf` and `conf/local.conf` exist, build with the standard BitBake target:

```sh
MACHINE=clk-mini-ultra-nor bitbake c2a-system-bundle
```

This is the expected target name based on the checked-in `c2a-system-bundle.bb` recipe; the repo checkout does not include a ready build directory or initialized submodule contents, so this command has not been run here. The layer set and build configuration must include the repo's `meta-carplay`, `meta-catplay`, `meta-carlinkit`, and `meta-carlinkit-mini` layers plus the pinned upstream dependencies. Confirm the deployed names from `meta-carplay/classes/c2a-artifact-names.bbclass`; the bundle recipe defaults to `tmp-c2a/${MACHINE}`.

The CatPlay recipe consumes `meta-catplay/recipes-apps/files/catplay.tar.gz`, not the app Git remote directly. On the future Linux builder, prepare it with `meta-catplay/scripts/create-src-bundle.sh /absolute/path/to/clean/catplay-checkout`, using a clean checkout at the intended app commit. The helper uses GNU tar and only excludes the root target directory; do not package the current tested checkout with nested experiment build directories. Record the generated archive hash. This source bundle was not generated here, and adding the host experiment does not enable a firmware feature.

Before building, check free space and inode capacity for the filesystem hosting `TMPDIR`, downloads, and sstate. The observed Mac had 17 GiB free and is not a practical clean Yocto builder. Use a Linux build host with adequate free space or a prepared cache. The user's adapter is confirmed as Ingenic X1600 AX1800M with 16 MiB NOR; the current measured CatPlay partitions are 128 KiB U-Boot, 3.5 MiB kernel, 11.5 MiB rootfs and 896 KiB userdata. The earlier 3 MiB/9 MiB/3968 KiB figures came from stock firmware and must not be used as the current layout. The read-only baseline measured 56,692 KiB Linux-visible RAM and 37,036 KiB available while powered from the Mac. Streaming memory pressure and radio performance remain unmeasured. See [device baseline](device-baseline.md).

For every successful build, record:

1. Both source revisions, submodule pins, host OS/architecture, BitBake/Yocto versions, and the full configuration diff.
2. Exact BitBake target and resulting artifact paths and byte sizes.
3. Root filesystem used/free bytes, the packed image size, and each partition capacity used for the comparison.
4. Whether the artifact was only built, statically inspected, run under emulation, or executed on physical hardware.

Do not flash artifacts as part of this procedure. There is no automatic flashing step in this guide.

## Trace and replay checklist

For each trace, state its origin and stage so data from independent sessions cannot be mistaken as one valid exchange. Prefer metadata and deterministic synthetic content for checked-in fixtures. Do not retain decrypted personal screen frames, keys, or other secrets in fixtures, logs, or the repository. If a real capture is needed for a local investigation, keep it access-controlled and out of version control; describe only privacy-safe metadata in the report.

1. **Source:** generated synthetic fixture, hand-authored request, or physical capture. For a physical capture, note date, device/phone identifiers at a privacy-safe level, firmware and iOS versions, transport, and capture point. Do not check in decrypted screen frames or key material.
2. **Protocol stage:** mDNS advertisement/discovery, RTSP `/info`, pair setup/verify, stream setup, screen/audio stream, teardown, or other. Include direction and per-session ordering.
3. **Framing:** raw TCP segmentation, reconstructed RTSP bytes, plist/XML or binary plist encoding, HTTP headers/body boundaries, RTP/RTCP packet boundaries, codec configuration, and timestamps. Preserve original bytes where permitted; document each transformation.
4. **Redaction:** identify removed keys or payload ranges and confirm lengths/offsets remain valid. Never silently concatenate packets or merge requests from different sessions. Keep capture bytes local and private.
5. **Result:** parser/harness version, command, expected behavior, actual output, and limitations. Label generated fixtures as generated; label physical captures as captured even when redacted or re-encoded.
6. **Cross-check:** only after a complete documented replay, compare a physical session stage by stage. Capture service advertisement, negotiated profile, session lifecycle, screen bytes, audio bytes, and teardown separately; a valid decode alone does not prove the phone received usable mirroring.

## Later supervised manual validation

The user has requested a temporary supervised adapter test. Prepare the compatible artifact and readiness checks before arranging their participation. That request does not authorize unattended execution, installation, reboot, settings changes or flashing.

1. Review the complete diff, host test outcomes, cross-build artifact sizes, partition fit, and rollback/recovery procedure. Stop if any of these are missing.
2. Confirm the user is present for the already-requested **temporary, reversible manual test**, with the exact adapter, phone, connection sequence, allowed observations and cleanup plan ready.
3. Use a temporary build from the pinned sources with CarPlay as the boot default and a user-invoked test switch. Do not overwrite persistent configuration. First verify startup, USB CarPlay operation, and return to the original behavior.
4. Enable the experimental profile only for one supervised session. Capture each protocol stage and resource observation separately. Stop on boot instability, session conflict, loss of CarPlay, or resource exhaustion; restore the known-good configuration/build immediately.
5. Report the result as a manual physical test with firmware hash and exact test conditions. It does not authorize flashing, publishing, or enabling the experimental mode by default.

## Current validation status

All 43 portable tests passed on Rust 1.99.1, aarch64-apple-darwin, on 19 October 2026. Both earlier synthetic replay executables also passed. The full protocol crate cannot build on this Mac because of the Linux-specific tracing code.

The first hosted Linux baseline compiled the protocol library and passed 76 of 77 library tests. The failing test expected PLL drift correction despite the existing production `DISABLE_PLL=true` setting. Its replacement tests the configured offset-only policy without changing production timing logic. A later hosted build found an unsupported `Instant.abs_diff` call in a new integration test; that test now uses explicit duration comparison.

The [successful hosted Linux run](https://github.com/keedle/catplay/actions/runs/39280228558) validated revision `6af0cb0dfcfdae6e6dcb81420c98cc3711dd9ed3` on Ubuntu 24.04 with Rust 1.99.1. The default configuration passed 89 library and eight integration tests. The experimental configuration passed 95 library and eight integration tests; its receiver probe built and passed two CLI tests. Both configurations passed the 43 portable tests and both synthetic smoke replays. Integration coverage includes real receiver admission/rollback, C2A disconnect lifecycle, encrypted screen conversion and authentication-tag refusal.

The application code is identical in later revision `886bb2ad9b930c14e9a31e54c3671033e7934001`. The intervening changes affect only the runner image and documentation. Earlier notes incorrectly treated a redundant queued retry as the only current evidence; the archived successful run is authoritative for this application code. These results do not validate MIPS execution, live Apple pairing, a decoder or car playback.

The user selected hosted builds and has no SDK or separate Linux builder. The [MIPS probe build plan](mips-probe-build-plan.md) records target ABI evidence and the requirements for a firmware-matched build. No MIPS artifact has been built. Vehicle-facing integration is now present in the reviewed source, with full application Linux validation pending. Read-only adapter inspection succeeded; no installation, configuration change or flash action occurred.

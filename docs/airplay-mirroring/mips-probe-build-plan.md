# MIPS build plan for the AirPlay receiver probe

Continuation note: this document retains the earlier audit/build history. See
[integrated mode](integrated-mode.md) for the current screen/audio implementation
and validation scope. Earlier statements about pending integration are historical.

This is a build-only plan for the experimental receiver metadata probe at `work/catplay/carplay/catplay_carplay/examples/airplay_mirror_probe.rs`. It does not authorize starting the listener on the adapter, installing an artifact, changing settings, or flashing firmware.

## Source and target facts

- Current CatPlay code revision: `70036b874150dbcd5313dc4a30adcf078202db93` on `feature/airplay-mirroring-poc`; firmware checkout: `aca307bb24efc7b1bcf624625e609a6666970761`.
- The board machine sets `DEFAULTTUNE = "mips32r2el"` and `DISTRO = "c2a-musl"` in `meta-carlinkit-mini/conf/machine/include/clk-mini-ultra/shared.conf`. Its kernel config selects little-endian, 32-bit MIPS32 Release 2 and the R4K FPU (`clk-mini-ultra-nor_defconfig`, `CONFIG_CPU_LITTLE_ENDIAN`, `CONFIG_CPU_MIPS32_R2`, `CONFIG_32BIT`, `CONFIG_MIPS_FP_SUPPORT`, `CONFIG_CPU_R4K_FPU`).
- Read-only device evidence in `work/device-evidence/target-abi.txt` identifies `/lib/ld-musl-mipsel.so.1` and `/usr/lib/libopus.so.0 -> libopus.so.0.11.1`. The captured first 4 KiB ELF header identifies ELF32, little-endian, dynamic MIPS32r2 with interpreter `/lib/ld-musl-mipsel.so.1`. Parsed `PT_MIPS_ABIFLAGS` in `work/device-evidence/mips-abi-fields.json` reports `e_flags=0x70001005`, `isa_level=32`, `isa_revision=2`, `gpr_size=1`, `cpr1_size=1`, `cpr2_size=0`, `fp_abi=1`, `isa_ext=0`, `ases=3`, `flags1=1`, `flags2=0`. GNU binutils defines FP ABI value 1 as **double-precision**. In a 32-bit ABI, this uses 32-bit FPU registers and pairs for double operations. Value 5 is the distinct FPXX mode. Preserve the raw fields when comparing objects and verify their meaning against GNU [MIPS FP ABI definitions](https://sourceware.org/binutils/docs/as/MIPS-FP-ABI-Variants.html).
- The natural Rust target is [`mipsel-unknown-linux-musl`](https://doc.rust-lang.org/nightly/rustc/platform-support.html): little-endian 32-bit Linux with musl, O32 ABI. The upstream [Rust target specification](https://github.com/rust-lang/rust/blob/master/compiler/rustc_target/src/spec/targets/mipsel_unknown_linux_musl.rs) defaults to MIPS32r2 with soft-float; the checked-in `Dockerfile.mips-musl-cargo-test` adds `+fpxx`. This is not yet a demonstrated match for the device ELF's `fp_abi=1`. A generic cross-build must inspect the probe and all relevant linked objects and resolve the FP ABI before it is called compatible.
- `Dockerfile.mips-musl-cargo-test` uses nightly, rust-src, `-Zbuild-std`, an MIPS musl cross GCC, and `RUSTFLAGS=-Ctarget-feature=+crt-static,+fpxx`. It explicitly warns that its `Instant::now()` implementation fails on the real device. A successful container build is only cross-compile evidence.

The target evidence supports O32/MIPS32r2/little-endian/musl and supplies the device ELF's FP ABI value. It does not establish that the generic musl cross image emits the same FP calling convention, sysroot, or linked system libraries as the firmware build. Keep this distinction in the build record.

## Smallest useful cross-build

The probe is an example in `catplay_carplay`, not the complete `catplay_c2a` application. It binds an AirPlay receiver and reports only metadata. This avoids compiling the framebuffer/UI, ALSA device interface, USB gadget, and Bluetooth product app. Build it with its explicit feature enabled:

```sh
cargo build --locked --release --target mipsel-unknown-linux-musl \
  -p catplay_carplay --example airplay_mirror_probe \
  --features experimental_airplay_mirroring
```

The `catplay_carplay` crate still depends on `catplay_audio` with its default AAC and Opus features. Those dependencies remain in the Cargo graph even though the picture-only probe rejects audio at runtime. For a generic cross image, the required target inputs are:

- Nightly Rust plus rust-src and the MIPS target standard library build, following the repository's MIPS musl Dockerfile (`cargo -Zbuild-std=std,panic_unwind,test build ...`). The project's Yocto Rust configuration sets panic-abort for target builds; the generic container's build-std wrapper is a test/build convenience and is not an exact reproduction of that setting.
- A MIPS little-endian O32 musl C/C++ compiler and linker. It must target MIPS32r2 and use an FP ABI compatible with the board and the prebuilt target libraries. Set Cargo's target linker plus matching `CC_mipsel_unknown_linux_musl`, `CXX_mipsel_unknown_linux_musl`, and `AR_mipsel_unknown_linux_musl`.
- An OpenSSL target build/sysroot. The existing Dockerfile builds OpenSSL 3.3.1 for MIPS musl and sets `OPENSSL_STATIC=1`; this is sufficient for its own container but may not match firmware's OpenSSL version or flags.
- Target libopus headers, library, and pkg-config metadata. `opus2` is configured for dynamic libopus. Use a MIPS musl build of the same Opus version/configuration as the firmware or the target sysroot from its Yocto SDK. The checked-in musl Dockerfile does not provide this dependency. The device's `libopus.so.0` confirms dynamic runtime linkage is available.
- A C++ compiler for `fdk-aac-sys`, which compiles its bundled FDK-AAC sources through the Rust `cc` build dependency; no separate system FDK-AAC library is needed for this Cargo dependency.

The target-specific flags used by the checked-in musl Docker recipe are `+crt-static,+fpxx`; the observed device executable has `fp_abi=1` (double-precision). Treat that distinction as unresolved until the cross-built executable and all linked objects have been inspected for compatibility. The firmware CatPlay recipe also adds static relocation, `-no-pie`, GNU BFD linker, and a static `libatomic` helper for MIPS. Preserve those MIPS-specific link requirements when testing a candidate artifact, while using the Yocto SDK to establish the exact target FP ABI. Do not copy GNU `mipsel-linux-gnu` packages into this musl target: both the C library ABI and dynamic loader differ.

## GitHub Actions paths

### Cross-compile smoke job

An Ubuntu 24.04 x86_64 runner can build inside a MIPS musl cross container. The current [rust-musl-cross image](https://github.com/rust-cross/rust-musl-cross) includes a nightly MIPS target and builds Rust's std for it, but CatPlay's `Dockerfile.mips-musl-cargo-test` is not a complete probe image: it lacks target libopus, tracks an unpinned nightly and base-image tag, and is explicitly marked as having a broken device timer. A repeatable CI image should pin the Rust nightly date, container digest, OpenSSL source checksum, Opus source version/checksum, and target compiler version. Populate a cross sysroot with musl OpenSSL and libopus, then run the Cargo build above with `--locked` and the target-specific link flags.

This job is valuable as a compile and ELF inspection gate. It must report the artifact as a **generic MIPS musl cross-build** until its toolchain and native libraries have been compared against the firmware SDK/device ELF metadata. Do not package this artifact for device use solely because the Rust link succeeded.

### Firmware-matched build job

The credible production-ABI path is to build the probe using the `clk-mini-ultra-nor` Yocto SDK generated from the pinned firmware layer revisions. That SDK supplies the machine's actual Rust target configuration, C/C++ compiler, libc headers/libraries, Opus, and OpenSSL sysroot. The full firmware `catplay_git.bb` recipe links MIPS non-PIE with GNU BFD and statically includes `libatomic`; it also depends on the regular CatPlay media library stack. For this isolated receiver example, use the SDK environment but compile only the `catplay_carplay` example rather than building `catplay_c2a` and the full image.

The user has no SDK or separate Linux builder and selected hosted builds. No configured BitBake build directory or SDK exists in the current checkout, and the Mac has about 16 GiB free. A clean Yocto build is not practical in this workspace. Generate the SDK on a larger Linux host with adequate space or a prepared downloads/sstate cache, then store it as a versioned CI artifact keyed by firmware commit, machine, and SDK checksum. Do not silently substitute the generic container for the machine SDK.

## Artifact checks before any later device proposal

On the Linux build runner, inspect the produced binary and its dynamic requirements without executing it:

```sh
file target/mipsel-unknown-linux-musl/release/examples/airplay_mirror_probe
readelf -h -A -l -d target/mipsel-unknown-linux-musl/release/examples/airplay_mirror_probe
```

Record and compare:

1. ELF32, little-endian, MIPS O32 ABI and MIPS32r2 ISA attributes; preserve device `e_flags=0x70001005` and `PT_MIPS_ABIFLAGS` fields as the comparison baseline.
2. FP ABI attributes against device `fp_abi=1` and the firmware SDK output. In GNU binutils, value 1 means double-precision ABI and value 5 means FPXX. Investigate any mismatch before treating the artifact as compatible; do not equate the target CPU's FPU presence with a matching calling convention.
3. Program interpreter and `DT_NEEDED` entries. Any interpreter must be present on the device; a dynamic Opus dependency must resolve to the expected `libopus.so.0`.
4. Binary byte size, build flags, Cargo.lock hash, Rust nightly date, cross compiler version, target sysroot identity and all native library versions.

The MIPS musl Dockerfile's `Instant::now()` warning is a known target-runtime limitation. Neither ELF inspection nor QEMU execution establishes that the timer, Wi-Fi interface binding, media timing, radio, RAM use, or session teardown behaves correctly on the adapter. Keep a successful result at the generic cross-build or ABI-inspected level. Any actual probe run requires its own explicit approval and supervised test plan.

## Known limits

- Device RAM is measured at 56,692 KiB total with about 36.2 MiB MemAvailable while idle; radio details remain unmeasured. Idle available memory does not establish peak headroom during an AirPlay session, so this cross-build cannot establish runtime capacity.
- Device ELF ABI fields are now recorded, but target-toolchain output has not been built or compared. Obtain a matching SDK ELF report before concluding any generic FPXX build is ABI-compatible.
- The current Mac has no Docker, Podman, Lima, MIPS compiler, or target sysroot available; no cross-build or emulator run was performed for this audit.

# AirPlay mirroring development result

Decision: pursue a constrained H.264 compressed-stream path first. Transcoding has not been shown necessary for this phone and car. The current change includes a host policy prototype, a feature-gated Linux receiver probe, and an opt-in integrated car-video path with a successful Linux application build. Screen-plus-audio and recovery changes are now under review. It is not installed adapter firmware or verified end-to-end mirroring.

The target is the user's working Carlinkit Mini Ultra 1, `clk-mini-ultra-nor`, confirmed Ingenic X1600 / AX1800M, paired with a Peugeot e208 GT EU and iPhone Air on iOS 27. A later read-only SSH check measured 55.4 MiB of Linux-visible RAM, about 36.2 MiB available while powered from the Mac, and the current 16 MiB NOR partition layout. The user reports firmware v0.17.0; its two release tags match our audited source baselines. No adapter configuration or software was changed. See the [device baseline](device-baseline.md).

## What is implemented

- A separate Rust policy experiment with explicit opt-in, mutually exclusive source leases, stale-callback rejection, H.264 descriptor checks, payload limits, configuration/IDR recovery and audio/control refusal.
- A host harness using the actual production video-framing source, with combined policy/converter regressions.
- A feature-gated picture-only receiver profile and bounded metadata probe, plus shared session-admission, cleanup and keyframe fixes.
- A source audit, architecture decision, integration sequence and reproducible host commands.

CarPlay remains the default. A feature-enabled application now offers a separate explicit process startup mode for experimental screen and media-audio mirroring; see [integrated mode](integrated-mode.md). Shared receiver lifecycle and keyframe fixes intentionally affect the default code path too; physical CarPlay regression testing remains outstanding. Separate artifact-only build recipes are provided; installed firmware and service settings remain unchanged. There is no dashboard mode switch. The experimental launch options select mirroring for the process lifetime; returning to normal CarPlay requires a fresh normal application session. No same-iPhone dual session, Dashboard window, remote touch or compositor is implied.

## Host experiment evidence

This table summarizes host evidence. Full continuation validation and its scope are recorded below, in the [receiver probe document](receiver-probe.md), and in the architecture review.

| Check | Result |
| --- | --- |
| Policy unit tests | 15 passed, including 4,112 deterministic arbitrary-input cases inside one test |
| Imported production video tests | 20 passed, including keyframe detection |
| New framing tests | 5 passed |
| Combined policy/converter tests | 3 passed |
| Both synthetic replay executables | Passed |
| Full protocol library on macOS | Build blocked by existing Linux-specific tracing APIs |
| Linux receiver probe | Build and CLI tests passed; see continuation evidence below |
| MIPS application or firmware build | Not performed; matching target artifact remains outstanding |
| Real iPhone trace, decoder acceptance, device throughput or end-to-end mirroring | Not tested |

The [successful hosted Linux run](https://github.com/keedle/catplay/actions/runs/39280228558) validated revision `6af0cb0dfcfdae6e6dcb81420c98cc3711dd9ed3` on Ubuntu 24.04 with Rust 1.99.1. The default configuration passed 89 library and eight integration tests. The experimental configuration passed 95 library and eight integration tests; its receiver probe built and passed two CLI tests. Both configurations passed the 43 portable tests and both synthetic smoke replays. Integration coverage includes real receiver admission/rollback, C2A disconnect lifecycle, encrypted screen conversion and authentication-tag refusal.

The application code is identical in later revision `886bb2ad9b930c14e9a31e54c3671033e7934001`. The intervening changes affect only the runner image and documentation. Earlier notes incorrectly treated a redundant queued retry as the only current evidence; the archived successful run is authoritative for this application code. These results do not validate MIPS execution, live Apple pairing, a decoder or car playback.

Astra made the architecture decision and independently reviewed source/tests. Sol implemented the protocol policy; Luna mapped firmware/build requirements. Root reviewed their evidence, found the cross-controller lease collision, integrated the replay tests and reran validation.

## What prevents a production switch

The source audit identified publication before admission, unguarded disconnect callbacks, CarPlay metadata overwriting the receiver identity, and assumptions about CarPlay events/keyframe control. The continuation repairs admission/cleanup, makes disconnect callbacks idempotent, and isolates the experimental profile metadata. The integrated source now connects the bounded picture bridge to the car-facing transmitter with separate ownership and control handling. The earlier integrated video-only application passed Linux compilation and tests in both modes at `ffb8fcf3e0c293e407e18094483b40713e679469`. New audio and recovery integration requires a fresh validation run. Physical iOS 27 negotiation, decoder acceptance and playback remain unverified. These findings are code-backed; no trace currently establishes the phone's negotiated stream or the Peugeot decoder's limits.

The next observation must identify the selected codec, coded dimensions/profile/level/cropping/rotation, initial versus later SETUP, timing mode and requested audio stream type using generated content. Until a trace and car capabilities are available, declaring a profile flip compatible would be speculation. See the architecture document for the precise integration prerequisites and later reversible test procedure.

Internet access during mirroring also remains unverified. A later test must distinguish local generated media from online content and record whether the iPhone retains cellular internet while joined to the adapter network. No routing, hotspot, Wi-Fi or DHCP settings were changed.

[Architecture decision](architecture.md) · [Receiver probe](receiver-probe.md) · [Baseline protocol audit](protocol-audit.md) · [Build and validation](build-and-validation.md)

## Integrated application continuation

Two Luna implementation workers completed receiver startup and a bounded video bridge under Sol supervision. Root and a fresh Astra reviewed integration, including real Annex B conversion, stale-session cleanup, car-screen ownership and unsupported recovery. New tests cover actual transmitter queueing and the two-frame outgoing cap. The updated CI builds the complete application with both default and experimental features. Source review and test definitions are not passing CI or vehicle-playback evidence.

The original MIPS probe attempt reached native compiler dependencies but hit its 120-minute limit before compiling the probe. No target binary was produced. Cache and time-budget repairs are separate from this application integration.

## Screen and audio continuation

The current work reuses the contributor's FairPlay receiver, audio decoders, PCM
bridge, car transmitter and firmware-managed access point. A combined H264/audio
profile adds exact live-car PCM format matching, audio stream ownership and
bounded video recovery. See [integrated mode](integrated-mode.md) for limits and
acceptance tests. Fully functional AirPlay has not yet been demonstrated on the
phone or adapter.

The next MIPS target builds only the integrated application and its native
dependencies. It retains the shipping MIPS ABI, musl time64 settings and pinned
layers. Bounded compiler task caching is implemented; cache hits and build-time
savings remain to be measured. The existing diagnostic compile remains separate
and is not restarted for application-only source changes.

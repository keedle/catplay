# AirPlay mirroring development result

Decision: pursue a constrained H.264 compressed-stream path first. Transcoding has not been shown necessary for this phone and car. The current change includes a host policy prototype and a feature-gated Linux receiver probe. It is not installed adapter firmware or verified end-to-end mirroring.

The target is the user's working Carlinkit Mini Ultra 1, `clk-mini-ultra-nor`, confirmed Ingenic X1600 / AX1800M, paired with a Peugeot e208 GT EU and iPhone Air on iOS 27. A later read-only SSH check measured 55.4 MiB of Linux-visible RAM, about 36.2 MiB available while powered from the Mac, and the current 16 MiB NOR partition layout. The user reports firmware v0.17.0; its two release tags match our audited source baselines. No adapter configuration or software was changed. See the [device baseline](device-baseline.md).

## What is implemented

- A separate Rust policy experiment with explicit opt-in, mutually exclusive source leases, stale-callback rejection, H.264 descriptor checks, payload limits, configuration/IDR recovery and audio/control refusal.
- A host harness using the actual production video-framing source, with combined policy/converter regressions.
- A feature-gated picture-only receiver profile and bounded metadata probe, plus shared session-admission, cleanup and keyframe fixes.
- A source audit, architecture decision, integration sequence and reproducible host commands.

CarPlay remains the default and production gadgets do not select the experimental profile. Shared receiver lifecycle and keyframe fixes intentionally affect the default code path too; physical CarPlay regression testing remains outstanding. Firmware recipes and installed service settings are unchanged. The code has no product mode switch. A future UI would select either normal CarPlay or full-picture mirroring and tear down the old phone session before starting the other. Resuming CarPlay requires a fresh session. No same-iPhone dual session, Dashboard window, remote touch or compositor is implied.

## Initial host experiment evidence

This table records the first phase. Continuation validation and its scope are recorded in the [receiver probe document](receiver-probe.md) and architecture review.

| Check | Result |
| --- | --- |
| Policy unit tests | 15 passed, including 4,112 deterministic arbitrary-input cases inside one test |
| Imported production converter tests | 17 passed |
| New framing tests | 5 passed |
| Combined policy/converter tests | 3 passed |
| Both synthetic replay executables | Passed |
| Full protocol library on macOS | Build blocked by existing Linux-specific tracing APIs |
| Linux/MIPS application or firmware build | Not performed; matching sysroot/build environment unavailable |
| Real iPhone trace, decoder acceptance, device throughput or end-to-end mirroring | Not tested |

Astra made the architecture decision and independently reviewed source/tests. Sol implemented the protocol policy; Luna mapped firmware/build requirements. Root reviewed their evidence, found the cross-controller lease collision, integrated the replay tests and reran validation.

## What prevents a production switch

The source audit identified publication before admission, unguarded disconnect callbacks, CarPlay metadata overwriting the receiver identity, and assumptions about CarPlay events/keyframe control. The continuation repairs admission/cleanup, makes disconnect callbacks idempotent, and isolates the experimental profile metadata. Vehicle-facing source selection and the remaining CarPlay control assumptions still need integration work. The AppleTV path also needs verification of iOS 27 setup/timing behavior and a deliberate audio policy. These findings are code-backed; no trace currently establishes the phone's negotiated stream or the Peugeot decoder's limits.

The next observation must identify the selected codec, coded dimensions/profile/level/cropping/rotation, initial versus later SETUP, timing mode and requested audio stream type using generated content. Until a trace and car capabilities are available, declaring a profile flip compatible would be speculation. See the architecture document for the precise integration prerequisites and later reversible test procedure.

Internet access during mirroring also remains unverified. A later test must distinguish local generated media from online content and record whether the iPhone retains cellular internet while joined to the adapter network. No routing, hotspot, Wi-Fi or DHCP settings were changed.

[Architecture decision](architecture.md) · [Receiver probe](receiver-probe.md) · [Baseline protocol audit](protocol-audit.md) · [Build and validation](build-and-validation.md)

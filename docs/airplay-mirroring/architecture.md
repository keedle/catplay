# CatPlay AirPlay mirroring architecture review

Status: architecture decision and isolated policy implementation approved for host experimentation. Production integration and device deployment remain unapproved and unverified.

Confirmed target supplied by the user: Carlinkit Mini Ultra1, firmware machine `clk-mini-ultra-nor`, Peugeot e208 GT in the EU, and iPhone Air on iOS 27. The existing working CatPlay installation uses the Ingenic X1600/XBurst MIPS32r2 platform and 16 MiB NOR. A later [read-only baseline](device-baseline.md) measured Linux-visible RAM and idle availability. Radio driver/device IDs were observed; radio performance and memory pressure during streaming remain unmeasured.

Reviewed source snapshots:

- CatPlay `bc6edae49554f2d0160b18ed18fc7b7376353115`.
- CatPlay firmware `aca307bb24efc7b1bcf624625e609a6666970761`.

## Decision

Build an isolated, explicitly enabled H.264 picture-source admission and ownership experiment. Exercise its real Rust implementation with host tests and exercise the actual upstream AVCC/Annex B conversion code through a host test harness. Do not enable an AirPlay listener, add a production mode toggle, modify existing firmware defaults, or claim end-to-end mirroring.

Retain the existing car-facing CarPlay transmitter as the eventual vehicle connection. A future mirroring receiver supplies full-screen encoded video to that transmitter through a separate source controller. It does not turn the vehicle into an AirPlay receiver. CarPlay remains the default phone-facing mode. At most one phone-facing source owns media delivery at a time.

Compatible compressed video may pass through without decoding. The existing receiver and transmitter share `EncodedVideoFrame`, and the code converts between length-prefixed AVC and Annex B representations. This proves a possible transport path, not compatibility between an arbitrary iPhone stream and the vehicle decoder. A transcoder is not justified by the available evidence. Composition, scaling, arbitrary rotation, and picture-in-picture remain separate work because they may require decoding and encoding.

## Existing source evidence

| Finding | Source location | Consequence |
| --- | --- | --- |
| Wired and wireless receiver gadgets instantiate `AirPlayReceiverProfile::CarPlay`. | `catplay_carplay_rx_gadget/src/server.rs:259-266`, `server_wireless.rs:430-437` | No current production mirroring mode is enabled. |
| An AppleTV profile advertises screen/FairPlay features and enables the multi-codec feature when `APPLETV_FORCE_HEVC` is true. | `carplay/catplay_carplay/src/carplay_rx/sink/receiver.rs:201`, `:363-385` | AppleTV is a useful starting point, but its advertised codec policy is unsuitable for an H.264-only experiment. |
| FairPlay setup derives AES media keys, and screen setup constructs a `ScreenReceiverSession`. | `receiver.rs:810-826`, `:598-641` | The source contains a real mirroring media receive path, beyond a service name. Successful operation with the intended iOS version is unverified. |
| Receiver video conversion produces an `EncodedVideoFrame`; sender conversion consumes that same type. | `screen/screen_frame_ext.rs:90-146`, `:43-46`; `screen/tx/screen_tx_session.rs:178-191` | H.264 rewrapping can preserve encoded NAL payloads. The two network sessions still require independent framing, keys and clocks. |
| The sender rejects length prefixes other than four bytes. | `screen/screen_frame_ext.rs:30-34` | The first experiment requires four-byte AVC lengths. |
| `CarManager` owns the car transmitter guard and `OverlayManager` owns the screen transmitter guard. | `c2a/catplay_c2a/src/proxy_rpc/car_manager.rs:29-41`, `overlay_manager.rs:23-29` | Future source selection belongs next to this existing ownership, not in a second competing car transmitter. |
| Overlay frames replace phone frames. | `overlay_manager.rs:119-145` | Existing UI is a whole-picture replacement mechanism, not a compositor. |
| A receiver publishes `sink.init()` before it acquires the shared session lock. `TxAdapter` publishes a connection from init and an untagged disconnect on drop. | `receiver.rs:860-869`; `proxy_rpc/tx_adapter.rs:144-156` | A rejected second connection can publish lifecycle events against the active peer. This must be fixed before concurrent listener admission. |
| `CarManager::on_patch_info` copies the car's feature flags, identity and display information into receiver `/info`. | `car_manager.rs:402-438` | A mirroring receiver needs its own info patching rules. Reusing the current sink can overwrite AppleTV features. |
| Only CAR sessions establish the events channel; the handle sends force-keyframe requests through that channel. | `receiver.rs:846-849`; `sink/session_handle.rs:72-76`, `:124-125` | Mirroring cannot rely on the existing CarPlay keyframe request. Waiting must have a deadline and close cleanly if no usable IDR arrives. |
| The handle reports `is_car = true` and `is_mirroring = false` unconditionally. | `sink/session_handle.rs:183-188` | Protocol identity must become explicit before mode-dependent behavior uses these methods. |
| The audio proxy only maps MainHighAudio to MainAudio. | `c2a/catplay_c2a/src/audiov2/audio_proxy.rs:50-52` | Mirroring GeneralAudio is not proven to map to car audio. The first experiment rejects mirror audio. |
| The receiver's screen codec permits a four MiB payload before reserving its input buffer. | `screen/screen_frame_codec.rs:153`, `:431-443` | Bounding a later policy queue does not bound total live adapter memory. A future receiver must enforce an ingress allocation limit too. |
| The receiver infers a keyframe from the first NAL only. | `screen/screen_frame_ext.rs:104-123` | The experiment must detect an IDR anywhere in an access unit. An eventual adapter must carry that result to the sender. |

Line numbers describe the reviewed snapshot and can move when code changes.

## Experiment contract

The independent experiment has no networking, Bluetooth, USB gadget, firmware install, device write, or audio output path. Its opt-in state is false unless explicitly enabled by a host caller.

A source lease identifies its issuing policy, source kind and unique session generation. Recreating the policy must not make an old lease valid again. A lease can own CarPlay or mirroring, never both. A competing acquire is rejected. Every release, configuration and frame submission must carry the same current lease. Old asynchronous callbacks must not affect a later owner. Generation exhaustion fails instead of reusing an identifier.

A mirroring lease starts without a usable configuration. It admits H.264 only, four-byte AVC lengths, nonzero dimensions matching the configured car-display experiment, explicit supported profile/level metadata, and zero rotation. Configuration parsing checks the portions it understands. The contract must state that metadata checks do not establish actual SPS dimensions, decoded output, or vehicle decoder support.

A changed configuration invalidates queued video and requires an IDR. A rejected new configuration also invalidates the old configuration. Each access unit is checked before the policy allocates its owned copy. It must contain well-bounded, structurally valid NALs. The first experiment rejects in-band SPS/PPS. Supporting them later requires exact matching against the accepted configuration or explicit reconfiguration. The policy identifies an IDR across all NALs, including when AUD or SEI comes first.

Both queued-frame count and owned payload bytes have explicit limits. The accounting includes retained configuration and any queued copies. Overflow cannot silently drop a reference frame and continue with dependent deltas. It clears queued media and returns to the configuration/IDR boundary, or closes the lease. Limits are experiment parameters, not measured safe limits for the Mini Ultra1. The 800 by 480 display and tiny payload limits used by tests are synthetic values, not measured Peugeot display capabilities.

Mirroring audio is explicitly unsupported. A future picture-only listener must advertise and reject streams consistently; whether the chosen iOS version accepts a video-only session requires observation. CarPlay phone audio must cease when its exclusive session ends. Mirror teardown releases its resources and makes ordinary CarPlay selectable again. It does not claim that an old CarPlay phone session automatically resumes.

No car HID, Siri, CarPlay modes, or iAP2 command is forwarded blindly into the mirroring session. Car-side protocol responses remain the responsibility of the car-facing connection manager.

## Production linkage, after the host experiment

1. Move peer publication until after session admission succeeds. Tag all connection, screen, configuration and teardown messages with the accepted source generation. Verify that a rejected connection and stale drop cannot affect the active peer.
2. Add an explicit mirroring receiver profile/configuration with H.264 advertisement and protocol identity. Preserve that profile in Bonjour and `/info`. Keep the CarPlay constructor and default selection unchanged. Suppress CarPlay invitations while mirror mode is selected.
3. Add a dedicated mirroring sink. It uses the existing screen receiver and applies the tested admission policy. It does not reuse the CarPlay command/audio forwarding implementation. Its receiver-input allocation limit and downstream queue limits must be budgeted together.
4. Hand accepted frames to the existing `OverlayManager` screen transmitter owner through a source-aware adapter. Carry configuration, true IDR detection, and timestamps. Ensure local waiting UI cannot replace an active mirroring frame unexpectedly. Do not create a second car transmitter.
5. Give mirroring a bounded wait for configuration, first IDR, car writability and teardown. Because the AppleTV path has no CarPlay event channel, an unavailable force-keyframe request cannot be the only recovery method.
6. Close mirroring on decoder incompatibility, unsupported timing/setup form, input overflow or car disconnect. Release its lease and queue. Restore CarPlay service availability according to the user's selected mode. Keep any error visible until acknowledged or the user selects another mode.

## Evidence needed before a device experiment

A sanitized observation from the iPhone Air on iOS 27 must establish the actual handshake shape for the Peugeot e208 GT target. Record whether streams appear in the initial SETUP or a later SETUP, the timing protocol, screen encryption, advertised and selected codec, length-prefix size, configuration dimensions, SPS profile/level, rotation changes and requested audio stream types. Retain protocol metadata only where practical. Do not retain pairing secrets, session keys or personal mirrored content.

The current source parses initial-SETUP stream information and timing-protocol information without showing full support for every possible form. Until a trace resolves this, a production implementation must reject unsupported forms explicitly instead of reporting success without starting a usable stream.

A target build needs the correct MIPS/XBurst configuration and existing firmware dependencies. The hardware identity already provided is sufficient to begin host work. Idle RAM has now been measured; before a deployment decision, obtain active memory-pressure evidence and a reproducible baseline build. The 16 MiB NOR layout limits image packaging; it does not reveal available RAM or prove a media queue safe.

## Proposed later hardware procedure

This is a proposed procedure, not authorization to execute it. No adapter writes, reboot, installation or flashing are authorized in the current task.

First establish the current working CarPlay baseline and retain its version/build identity. Prepare a complete build artifact and recovery procedure for review. Review target binary size, dependency changes, ingress and queue budgets before asking for deployment authorization.

If a later explicitly authorized bench run proceeds, keep the vehicle stationary. Start with an unprotected, locally generated H.264 test image/video at the measured car display dimensions. Confirm successful configuration, first IDR, sustained frames and timing without decoding/transcoding on the adapter. Record memory, CPU, frame drops and timeouts. Test phone disconnect, car disconnect, rejected second source, repeated mirror entry/exit, configuration change and failure before the first frame. Verify ordinary CarPlay still starts after mirror shutdown and after reboot.

Audio, protected content, portrait/full-screen adaptation, UI composition and simultaneous same-phone sessions require separate acceptance criteria. No host test in this change can establish them.

## Licensing observation

No repository-root LICENSE or COPYING file was found in the two reviewed checkouts. Some firmware recipes declare `GPL-2.0-only`; third-party components carry their own notices. Those facts do not establish a uniform license grant for all first-party source. Do not add a guessed project license or describe the whole repository as GPL. Any release/distribution plan needs the maintainer's applicable terms established.

## Review outcome

Independent source review approved the isolated experiment after two fixes. Leases now include an issuing-policy identity so an old lease cannot match a newly constructed policy. Successful reconfiguration now clears old queued payloads and configuration before allocating replacements.

The reviewer independently ran all 15 policy tests and the explicitly enabled synthetic smoke replay. They passed. The reviewer also independently ran the source-import replay tests, with 17 imported upstream tests, five added framing tests and three policy-to-converter tests passing. The final independent run passed all 40 tests across the two experiments. The policy tests include 4,112 deterministic arbitrary-input cases to check panic and payload-bound behavior. No production source file changed in the reviewed experiment scope.

The production video-conversion replay imports the existing source modules directly. Its synthetic NAL fixtures can establish byte preservation and malformed-length rejection, but they are not decoder-valid pictures. Tests for one- and two-byte AVC lengths exercise the converter only; the production screen sender requires four-byte lengths.

The full `catplay_carplay` library test attempt on macOS fails at existing Linux-specific low-priority queue APIs, including `SYS_gettid` and the Linux `pthread_setname_np` signature. No broad platform-portability rewrite is part of this change. The validation boundary is explicit. Policy and converter tests have passed on the host. A full application build did not pass on this macOS host, a MIPS target image was not built, and no physical end-to-end test ran. A successful policy or conversion test does not mean AirPlay mirroring currently works in the vehicle.

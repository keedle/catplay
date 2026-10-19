# CatPlay AirPlay mirroring protocol audit

Read-only source audit at upstream `bc6edae49554f2d0160b18ed18fc7b7376353115`, branch `feature/airplay-mirroring-poc`, 19 October 2026. Paths below are relative to the CatPlay repository root. This source audit preceded the later [read-only device baseline](device-baseline.md). No flashing occurred. Source implementation is evidence of code, not proof that a current iPhone and this head unit complete the exchange.

## Finding

CatPlay contains a substantial AppleTV-style mirroring receiver, including FairPlay key unwrap and encrypted screen framing. The production wireless gadget selects the CarPlay profile. A simple profile flip would expose protocol and bridge assumptions. The existing encoded-video API can adapt a compatible compressed stream without decoding it. No measured evidence currently establishes that this user's iPhone/head-unit pair requires transcoding.

## Instructions and license inventory

No `AGENTS.md` was found in the repository or the inspected ancestor directories from `/` through the workspace. `git ls-files '*AGENTS.md' '*LICENSE*' '*COPYING*'` returns only `core/catplay_hap/asm/LICENSE.cryptogams` and `LICENSE.openssl`. Root and `catplay_carplay` Cargo manifests do not declare a project license. Individual HAP files carry MIT or Apache headers. This is a factual inventory, not a conclusion about permission to distribute. The user authorized personal GitHub forks and development. No upstream submission or release is part of this change.

## Receiver coverage and gaps

| Area | Implemented source evidence | Limits and unverified behavior |
| --- | --- | --- |
| Discovery | `carplay/catplay_carplay/src/carplay_rx/sink/receiver.rs:344-424` selects distinct AppleTV/CarPlay feature sets, model, device ID, and TXT values. `sink/server.rs:88-95` registers Bonjour. | Production `catplay_carplay_rx_gadget/src/server_wireless.rs:426-439` selects `CarPlay`. AppleTV uses device ID suffixed `_atv` at receiver line 412, but no interoperability capture validates that identity. Service host/name remain `carplay.local.`/`carplay` in server line 91. |
| Codec negotiation | AppleTV advertises SCREEN, ROTATE, VIDEO_FAIRPLAY and AUTHENTICATION4 at `receiver.rs:363-385`. | `APPLETV_FORCE_HEVC=true` at line 201 causes SCREEN_MULTI_CODEC advertisement at 383-385; `CARPLAY_FORCE_HEVC=false` at 200. Advertising multi-codec support is not proof the peer chooses HEVC or that the car lacks it. |
| Pairing/control | `/pair-setup`, `/pair-verify`, `/auth-setup`, `/fp-setup` dispatch at `receiver.rs:295-298`. `pairing/pairing_helper_rx.rs:114-213` implements TLV8 HAP setup/verify and transitions to control encryption. | Pair setup uses `CARPLAY_MAGIC_PIN` at helper line 124. AppleTV does not advertise HAP pairing at receiver line 374. No legacy binary pair-verify parser or user PIN UI was found in this helper. No sender interoperability run exists. |
| FairPlay | `pairing_helper_rx.rs:248-255` handles fp-setup. `carplay/catplay_fairplay/src/lib.rs:69-114` handles 16/164-byte version-3 messages and decrypts 72-byte wrapped keys. `receiver.rs:810-823` installs resulting AES material. | Only the implemented handshake version/lengths are supported. No modern DRM-protected content support should be inferred. |
| Session setup | `receiver.rs:554-571` handles subsequent stream SETUP; `:601-642` opens reverse TCP screen receiver; `:763-898` sets up cipher, timing, keep-alive and sink lifecycle. | `InitialSetup.streams` exists at `msg/initial_setup.rs:22` but initial SETUP does not process it and replies with streams=None at receiver line 882. `timingProtocol` is parsed at initial_setup line 44 but receiver always uses UDP TimingClient at 840-844. Unknown encryption can remain Unconfigured and later derive screen AES from zero material, because screen_frame_codec lines 174-175 treat Unconfigured/None that way. Fail-closed validation is a separate hardening task. |
| Screen cryptography | `screen/screen_frame_codec.rs:164-181` selects AES-CTR or ChaCha; `cipher/aes.rs:3-13` derives stream key/IV from session key and streamConnectionID; codec lines 398-478 support progressive frame reception and a 4 MiB payload maximum. | Different session encryption is an adaptation cost, not a codec-transcoding requirement. Default AES prefetch reserves 512 KiB, codec line 158. Idle RAM was subsequently measured; streaming memory pressure and CPU throughput remain unmeasured. |
| Screen payload/config | `screen/rx/screen_rx_session.rs:69-157` handles config, video, keep-alives and callbacks. `screen/screen_frame_ext.rs:178-217` parses AVCC or format-description AVCC/HVCC. | ClearScreen is ignored at rx_session 131-134. Keyframe detection checks only the first NAL byte at screen_frame_ext 104-123. Access units starting AUD/SEI/SPS before IDR may therefore be misclassified. |
| Timing | `clock/timing_client.rs:28-34,53-64,105-154` does startup timing pings, updates MediaClock, and times out. `screen_frame_ext.rs:96-102,137` maps sender timestamp into local Instant. | If RespectTimestamps is clear, PTS becomes now. No A/V synchronization measurement exists. TimingProtocol variants are not negotiated. |
| Audio | `receiver.rs:644-751` opens codecs/RTP/sinks; `:754-758` accepts AudioLegacy and modern audio. `msg/stream_desc.rs:76-94` maps legacy audio, enforces >=250 ms latency. Sink callback is decoded i16 PCM at `carplay_rx/receiver_api.rs:198-210`. | AppleTV advertises PCM/AAC-LC/AAC-ELD/ALAC at receiver 363-378, yet the c2a car bridge has no proved matching mirroring audio setup. `c2a/catplay_c2a/src/audiov2/audio_proxy.rs:50-52` maps MainHighAudio to MainAudio but leaves GeneralAudio=96 unchanged. It sends that stream type to car setup at 59-65. Avoid advertising working mirror audio in an initial video-only PoC. |
| Teardown | `receiver.rs:455-496` tears down individual screen/audio streams or requests session closure; derived shutdown covers streams/sink at 80-96,113-145; `screen_rx_session.rs:167-170` shuts down its sink. | Screen errors are not reconciled into receiver close, receiver line 127 comments that reconciliation out. `sink/server.rs:122-125` shutdown is TODO. Receiver can therefore keep control alive after a failed screen. Full repeated-session behavior requires tests. |

## Existing compressed forwarding path

1. `screen_rx_session.rs:84-87,127` calls `ScreenFrame::video_decode` and passes `EncodedVideoFrame` into the sink.
2. `screen_frame_ext.rs:91-146` scans NAL lengths, changes AVCC/HVCC length prefixes to Annex B start codes, and returns compressed bytes, config, dimensions, keyframe flag, local PTS, and reusable buffers. It does not decode pixels.
3. `c2a/catplay_c2a/src/proxy_rpc/proxy_screen.rs:69-90` sends frames through a channel and waits for consumption with a 500 ms timeout. `overlay_manager.rs:119-145` forwards a selected iPhone frame to the car stream.
4. `screen/tx/screen_tx_proxy.rs:98-140` accepts the same EncodedVideoFrame API, sends config if changed, requires a known keyframe initially, and applies frame-queue backpressure.
5. `screen/tx/screen_tx_session.rs:178-191` serializes config then calls `ScreenFrame::video_proxied`. `screen_frame_ext.rs:44-46,66-85` restores length prefixes and writes PTS using the destination media clock. Separate destination ScreenFrameCodec encrypts that screen connection. Only NAL framing and protocol timestamps change.

## Car-facing ownership and integration traps

`CarManager` owns the live car `TeardownGuard<AirPlayTransmitterProxyRef>` and `OverlayManager`, initializes display dimensions/fps from car `/info` at `proxy_rpc/car_manager.rs:78-109`, and creates/drops the car screen guard according to mode arbitration at 266-318. `OverlayManager` owns `TeardownGuard<ScreenTransmitProxy>` at `overlay_manager.rs:29,71-76`. Injecting mirroring must go through this owner, rather than open a competing screen sender.

`CarPlayRxSession` callbacks at `proxy_rpc/rx_bridge.rs:53-123` forward into `TxAdapter`; its drop at 143-149 notifies disconnect. Audio car setup is queued by CarManager at 352-370 and owned through the PCM proxy guard, `audiov2/audio_proxy.rs:35-71`.

Known assumptions to address:

- `CarManager::on_patch_info`, lines 402-438, overwrites receiver `features` with `car.features` at 432 and model at 428. Reusing it verbatim can turn an AppleTV `/info` response back into CarPlay and disagree with Bonjour. Copy only chosen display/audio facts for mirroring.
- AppleTV initial setup does not create the CarPlay events channel, `receiver.rs:846-849`; `session_handle.rs:72-77` therefore returns EventsUnsupported for commands. Existing overlay keyframe requests cannot recover missing mirror IDRs through this API. Do not claim that waiting after an arbitrary source switch is guaranteed to work.
- `session_handle.rs:183-188` always reports `is_car=true`, `is_mirroring=false`, regardless of selected profile.
- `sink/server.rs:100-114` creates a CarPlay inviter without a profile guard, and wireless manager drives invitations at `server_wireless.rs:506-520`. A mirror listener must never start CarPlay invites.
- Modes/commands/HID and initial permanent screen ownership are CarPlay concepts in existing manager. Mirror callbacks need explicit source selection and lifetime handling.
- `screen_frame_ext.rs:31-33` only transmits NAL length width 4, although receive config may contain other widths. Normalize framing or reject known incompatible widths before forwarding.

## Adaptation versus transcoding decision

Proven differences handled by existing code are AES/ChaCha session encryption, stream IDs, AVCC/HVCC/Annex B framing and media clock translation. None requires pixel decoding.

A real codec mismatch, unsupported H.264 profile/level/reference structure, incompatible coded dimensions/cropping/rotation, or head-unit inability to scale the stream can require changing the compressed elementary stream. Those are conditional requirements. The source does not establish any of them for this actual iPhone and car. AppleTV's current multi-codec advertisement creates a risk of HEVC ingress, but disabling that advertisement is the first experiment, not proof that a transcoder is mandatory.

An initial gate can reject known HEVC and dimension/NAL-format mismatch. Dimension comparison alone does not prove compatibility. SPS/profile/level/cropping/reference checks and captured head-unit acceptance remain required for a broad compatibility claim. Rotation/letterboxing cannot be achieved by changing only header width/height. For target X1600 AX1800M Mini Ultra1, the user's 16 MiB NOR fact constrains firmware size; it provides no RAM or codec-acceleration measurement. No target throughput or encoder capability claim is established here.

## Accepted scope and implemented host experiment

Astra's architecture review rejected production activation at this stage. The accepted implementation is a standalone, dependency-free, private package at `experiments/airplay-mirroring`, excluded from the firmware workspace by its own `[workspace]`. Production CarPlay and AppleTV code remains unchanged. The experimental CLI requires `--enable-mirroring` to execute its synthetic replay.

The package implements explicit source leases with process-unique issuer identity and checked generations, cross-instance/stale token refusal, H.264 observed-metadata admission, exact configured dimensions, zero rotation, AVCC4 framing checks, actual IDR inspection across AUD/SEI, configuration-before-IDR and dependency resets, bounded retained/queued compressed payloads, typed audio/control refusal and teardown. Rejected configuration invalidates old media. In-band SPS/PPS is conservatively rejected. It preserves opaque timestamp values exactly; it performs no real media-clock mapping. Synthetic fixtures are not claimed to be decodable video. Unit tests cover ownership races, incompatibility, malformed framing, overflow, reconfiguration, passthrough byte preservation and payload accounting.

The user's hardware context is Peugeot e208 GT EU, iPhone Air on iOS 27, and Carlinkit Mini Ultra1 `clk-mini-ultra-nor`. A later Mac-powered idle RAM/process snapshot is recorded in the device baseline. No actual head-unit decoder capabilities, streaming resource measurements or sender captures have been obtained. All caps and size limits in the replay are synthetic test parameters.

The following were the original proposal and remain later integration work, not completed production behavior:

- Keep normal CarPlay behavior as default. Add an explicit opt-in H.264 mirroring receiver profile/config that preserves mirror identity in `/info`, suppresses CarPlay invitations and does not advertise unsupported audio or HEVC.
- Use existing car screen ownership with an explicit mirror source-selection lifecycle and reject incompatible compressed configs. Prefer failing with a stated reason to passing an unsupported frame to the car. Do not silently claim config gate guarantees head-unit acceptance.
- Host-test AES screen input through real config/video parsing, then through compressed forwarding and independent ChaCha screen output. Assert exact SPS/PPS and IDR/delta NAL preservation, fragmented input reception, destination timestamp translation and teardown/restart behavior.
- Test advertisement/profile separation, unsupported config rejection, and legacy audio handling explicitly. A video-only PoC should reject audio requests and document that choice.
- Treat current iPhone discovery/pairing/connectivity, real head-unit decoder acceptance, A/V sync and X1600 resource usage as later hardware validation, never as host-tested success.

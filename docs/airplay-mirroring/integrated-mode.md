# Experimental car-screen mirroring mode

Status: the earlier video-only integrated application passed Linux validation at
`ffb8fcf3e0c293e407e18094483b40713e679469`. Audio and recovery changes are under
review and require a new Linux validation run. MIPS and physical validation remain pending.

This is the integrated application's experimental screen and media-audio mode. It is
separate from `airplay_mirror_probe`, which discards video and cannot display a
picture in the car. Source integration, a successful build, phone negotiation,
and visible vehicle playback are separate milestones. No physical result is
claimed by this document.

## Selection and scope

Build `catplay_c2a` with `experimental_airplay_mirroring`. Normal application
startup with no arguments retains CarPlay. Mirroring requires all three
explicit launch options:

- `--experimental-airplay-mirroring`
- `--mirror-h264-profile-idc` with an explicitly selected profile value
- `--mirror-h264-level-idc` with an explicitly selected level value

The profile and level are admission gates for the SPS header, not discovered
Peugeot decoder capabilities. Choose them from test evidence; do not present a
made-up combination as compatible. Missing, duplicate or unsupported options
are rejected. A normal build cannot enable the mode through arguments alone.

The integration selects one phone-facing mode for the process lifetime. It is
not a dashboard toggle or a same-phone simultaneous CarPlay/AirPlay session.
Returning to CarPlay uses a fresh normal application session. The existing
car-facing CarPlay transmitter remains responsible for USB output.

The experimental phone listener uses the configured network interface and
in-memory pairing storage. It does not set up Bluetooth, iAP2 or a new Wi-Fi
network. A suitable existing network connection is therefore required. Raw
protocol tracing is disabled in mirroring mode.

Compatible H264 video passes through without decoding or transcoding. Media audio
reuses the existing AAC/ALAC/PCM receiver and PCM car-output bridge. The listener
advertises audio only when the live car offers a matching decoded PCM format.
There is no resampler. Remote touch, Siri, video scaling and composition are
outside this mirroring mode.
The car display and a live car connection must be available before a usable
mirror receiver is offered. The mode acquires the car screen only for an active
mirror session and returns it on disconnect. Frame queues and session ownership must remain
bounded across reconfiguration and disconnects.

Video recovery waits up to three seconds for a natural IDR after a car keyframe
request or a compatible configuration change. During the wait, dependent frames
are dropped and stale queued media is cleared. The deadline also expires on a
silent stream; repeated requests cannot keep extending it. No unverified AirPlay
keyframe command is sent. An unsupported codec/profile/level or an expired wait
still closes the mirror session. Physical testing must establish whether the
phone supplies IDRs promptly enough for the head unit.

Audio setup carries both a phone-session identity and a unique stream-attempt
identity, so a late setup result cannot attach to a replacement stream. The car's
MainAudio resource is acquired for playback and released on teardown or failure.
Normal RTP pause/resume must retain its source, and stream teardown must allow a
new setup in the same phone session. These paths require both host regression
coverage and supervised phone testing.

Firmware starts the existing access point independently of the CatPlay process.
The experimental listener reuses that network interface. This source finding
avoids a second network manager; actual phone discovery is still unverified.

## Validation sequence

1. Run source tests for default selection, explicit opt-in, session ownership,
   video bounds, reconfiguration and disconnection. Include the actual
   receiver conversion, bridge and transmitter conversion in the video test.
2. Build the complete Linux application and run its library tests in both
   default and experimental configurations. A protocol-only pass is not an
   application build result.
3. Produce a firmware-matched MIPS application and compare its ABI, loader,
   dependencies, size and source identity against the recorded adapter.
4. The separate diagnostic probe can establish real iPhone negotiation and
   stream metadata without the car. It does not display a picture and is not a
   prerequisite if the integrated artifact is ready for a supervised car test.
5. Conduct a supervised integrated test with the car connected. Observe a
   generated test picture with sound, synchronization, audio pause/resume, video
   recovery, disconnect/reconnect behavior and normal CarPlay recovery. A Mac-powered diagnostic alone cannot verify the Peugeot display.

The running MIPS probe build uses its existing pinned source revision. Work on
this integration does not replace that source or turn its output into the new
application. A subsequent reviewed build is required for integrated mode.

No firmware flashing, persistent installation, service restart or adapter
execution is part of the host development checks described here.

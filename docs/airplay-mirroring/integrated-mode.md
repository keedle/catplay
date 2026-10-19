# Experimental car-screen mirroring mode

Status: implementation and review in progress; integrated Linux and physical validation pending.

This is the integrated application's picture-only development mode. It is
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

Only compatible H264 picture data is intended to pass through. Audio, remote
touch, Siri, scaling, rotation and composition are outside this first mode.
The car display and a live car connection must be available before a usable
mirror receiver is offered. The mode acquires the car screen only for an active
mirror session and returns it on disconnect. Frame queues and session ownership must remain
bounded across reconfiguration and disconnects.

The first implementation closes the mirror session when the car requests a
fresh keyframe, downstream delivery fails, or the video configuration changes.
The picture-only phone session has no CarPlay event channel for requesting an
IDR frame. Reconnecting is required in these cases; seamless recovery and
rotation are not implemented. These restrictions may limit real head-unit
compatibility and must be assessed in the supervised test.

## Validation sequence

1. Run source tests for default selection, explicit opt-in, session ownership,
   video bounds, reconfiguration and disconnection. Include the actual
   receiver conversion, bridge and transmitter conversion in the video test.
2. Build the complete Linux application and run its library tests in both
   default and experimental configurations. A protocol-only pass is not an
   application build result.
3. Produce a firmware-matched MIPS application and compare its ABI, loader,
   dependencies, size and source identity against the recorded adapter.
4. Use the separate diagnostic probe to establish real iPhone negotiation and
   stream metadata. This probe does not need the car and does not show a picture.
5. Conduct a supervised integrated test with the car connected. Observe a
   generated test picture, disconnect/reconnect behavior and normal CarPlay
   recovery. A Mac-powered diagnostic alone cannot verify the Peugeot display.

The running MIPS probe build uses its existing pinned source revision. Work on
this integration does not replace that source or turn its output into the new
application. A subsequent reviewed build is required for integrated mode.

No firmware flashing, persistent installation, service restart or adapter
execution is part of the host development checks described here.

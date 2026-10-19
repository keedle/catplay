# Read-only adapter baseline

Observed on 19 October 2026 over SSH, with the Mini Ultra 1 plugged into the Mac rather than the car. No service restart, configuration change, installation, reboot or flashing was performed. The earlier connection attempt timed out; the retry after the user connected the adapter succeeded using existing authentication.

## Installed system

- Kernel reports `Linux clk-mini-ultra-nor 6.18.36`, MIPS, PREEMPT, build timestamp `Fri Jun 19 17:54:15 UTC 2026`.
- `/proc/cpuinfo` reports `X1600`, `Carlinkit Mini Ultra (AX1800M)` and Ingenic XBurst with MIPS32r2 support.
- `/usr/bin/catplay_c2a` is 10,635,616 bytes. SHA-256 is `4cf68394e19c428f9edf70f1c015c1b7c38f2b9965dd1759942b563cb16d6898`.
- The user identifies the installed firmware as **v0.17.0**. The [published release](https://github.com/catplay-labs/catplay/releases/tag/v0.17.0) and local release tags resolve to app `bc6edae49554f2d0160b18ed18fc7b7376353115` and firmware `aca307bb24efc7b1bcf624625e609a6666970761`, exactly the source baselines audited here. This is release/tag provenance; the installed binary hash has not been compared against the release archive. Kernel build and executable file timestamps alone do not identify its source commit.
- The CatPlay process is running. The USB gadget reports `configured`; this is the Mac USB connection and does not establish a vehicle CarPlay session.
- AIC drivers are loaded. SDIO reports `C8A1:0082` with `aicwf_sdio` and `C8A1:0182` with `aicbsp_sdio`. Exact silicon revision and radio performance were not independently established.

## Memory snapshot

Linux reports KiB units under the `kB` label:

| Metric | Observed |
| --- | ---: |
| MemTotal | 56,692 KiB, approximately 55.4 MiB |
| MemAvailable | 37,036 KiB, approximately 36.2 MiB |
| MemFree | 21,268 KiB, approximately 20.8 MiB |
| SwapTotal | 0 |
| CatPlay VmRSS, later process sample | 7,028 KiB, approximately 6.9 MiB |
| CatPlay VmHWM | 7,044 KiB |
| CatPlay VmSize | 26,308 KiB |
| CatPlay threads | 5 |
| System load average | 0.00 / 0.00 / 0.00 |

These are a Mac-powered baseline. They do not measure CarPlay or mirroring memory pressure, throughput or CPU utilization under load. `MemTotal` is Linux-visible usable RAM, not a direct measurement of installed physical DRAM capacity. Virtual address space is not resident usage. Values changed slightly between the two reads.

## Current flash layout

The installed CatPlay kernel exposes this layout, distinct from the earlier stock firmware report:

| Partition | Hex size | Capacity |
| --- | --- | ---: |
| uboot | `00020000` | 128 KiB |
| kernel | `00380000` | 3.5 MiB |
| rootfs | `00b80000` | 11.5 MiB |
| userdata | `000e0000` | 896 KiB |

Total NOR remains 16 MiB; current erase size is 4 KiB. `/persist` reports 84 KiB used and 812 KiB available. The base `/old_root` filesystem reports no free space; `/` uses a RAM-backed writable overlay. This means writable root space must not be mistaken for spare persistent flash. No image-size conclusion follows from the executable's uncompressed size alone.

## Effect on the architecture

The measured idle memory removes one uncertainty, but it does not justify a software transcoder or larger media queues. Keep the H.264 compressed-stream experiment and explicit ingress/queue limits. Memory must be measured again during a real car session, including crypto buffers and outbound backpressure.

No head-unit display capabilities, iOS 27 mirroring handshake, encoded screen trace, audio behavior or vehicle playback were collected. Those still require the relevant live sessions. The Mac connection is sufficient for system inspection and can support later receiver work once an approved test build exists; the car is required for negotiated car capabilities and end-to-end validation.

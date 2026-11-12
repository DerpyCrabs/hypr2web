# hypr2web

View/control an existing Hyprland monitor in a browser, without RDP.

Default path: Wayland screencopy → CUDA scale/color conversion → NVIDIA NVENC →
WebRTC (H.264 over encrypted UDP) → browser video element.
HTTPS WebSocket carries signaling, input and telemetry. A WebSocket/WebCodecs video
mode remains available for comparison and networks that block UDP.

## Build and run

Requires Rust, a C compiler, pkg-config, FFmpeg development libraries
(libavcodec, libavutil, libswscale), libxkbcommon, NVIDIA driver/NVENC and the CUDA toolkit.
On Arch: base-devel, rust, ffmpeg, libxkbcommon, cuda and the NVIDIA driver.
CUDA defaults to /opt/cuda; set CUDA_HOME if installed elsewhere. Build compiles
the conversion kernel to embedded PTX; no runtime nvcc process.

    cargo build --release
    ./target/release/hypr2web --bind 127.0.0.1:8085 --output HDMI-A-1 --cert ui.crt --key ui.key

Run inside Hyprland. For tablets bind to the LAN IP and use HTTPS with a trusted
certificate. WebRTC uses dynamically allocated UDP host candidates: allow LAN UDP
traffic. No external STUN or TURN service is contacted.

This prototype has **no authentication**; anyone able to reach it can view/control
the desktop. Use a trusted LAN. One viewer/controller at a time. A second connection
is rejected until the first disconnects. It mirrors the existing monitor with cursor.

## Interface

The rdp2web-style icon panel hides at the upper edge. Hover/tap the strip to reveal it.
Icons: reconnect, disconnect, release input, keyboard, statistics, settings, fullscreen.
Settings: transport, FPS, bitrate ceiling, automatic quality, compositor keyboard layout.
WebCodecs decoder preferences appear only for the WebSocket transport. Auto tries
hardware support first and falls back to the browser's default decoder.

Stream resolution automatically fits the client's available viewport in device pixels
(CSS size × devicePixelRatio), preserving the monitor's aspect ratio and never upscaling.
Rotation, window resizing and the system keyboard update the encoder in the existing
session after a 300 ms debounce. There is no manual resolution selector. Capture still
reads the monitor at its native size; scaling happens before video encoding/transmission.

Automatic quality is disabled by default. The experimental opt-in mode can further
reduce resolution/FPS on slow decoding and adjust bitrate. A viewport change resets
the resolution ceiling to the new client dimensions.

Bitrate/FPS/transport preferences persist in this browser.
The screen accepts pointer/touch and keyboard input when focused. On tablets,
tap to click, drag with one finger, and swipe with two fingers to scroll vertically.
Lift both fingers before starting another click or drag. The keyboard button in the top toolbar focuses an invisible input and opens the device's
system keyboard (for example Samsung Keyboard on Android). Tap a remote text field
first, then open the keyboard. Text, IME commits, Backspace and Enter are forwarded;
the desktop fits above the system keyboard. Close with Android Back or the keyboard button. No input field is shown on the page.
No custom on-screen key grid is used. EN/RU in Settings still select the compositor
layout for physical keyboard input.

Diagnostic URL options:

- /?stats=1 — show overlay (also toggled with the graph icon).
- /?transport=webrtc — use WebRTC with automatic viewport sizing.
- /?transport=ws — use WebSocket/WebCodecs video.
- /?gpu=0 — compare CPU conversion against CUDA (NVENC still encodes).
- /?adaptive=0 — explicitly keep quality fixed (the default).
- /?adaptive=1 — opt into experimental automatic quality.

## Pipeline and adaptation

- CUDA bilinear scale + BT.709 limited-range RGB→NV12 in one kernel; NVENC consumes
  the CUDA frame directly. Captured BGRA/RGBA is still copied from shared RAM to GPU.
- The shared primary CUDA device is cached for process lifetime. Per-session frames,
  encoders and kernels are freed; resolution changes retire the old encoder first.
- NVENC p1/ultra-low-latency, baseline H.264, no B frames/lookahead, CBR with max rate
  and two-frame VBV. CQ is intentionally unset: FFmpeg CQ overrides average bitrate
  and VBV even if RC was configured as CBR. Bitrate is a rate-control ceiling, not
  a guarantee of uniform packet spacing or total link bandwidth including overhead.
- WebRTC has NACK retransmissions and PLI/FIR keyframe recovery. Browser jitter-buffer
  target is requested as zero where supported; actual buffering remains browser-controlled.
- Video no longer waits for a per-frame application ACK on the UDP path. The previous
  two-frame/80ms roundtrip window could cap throughput around 25 fps.
- WebSocket retains its two-frame ACK window, but draws immediately after decoding,
  eliminating the additional requestAnimationFrame wait.
- Capture/output and input queues are bounded; input is independent of video.
- Once-per-second browser feedback lowers bitrate on loss >2%, jitter buffer >60 ms
  or RTT >100 ms. Quiet periods recover 5% every five samples up to the chosen ceiling.
  A cooldown limits changes. Three slow decode samples lower resolution/FPS.
- Held input is released on disconnect. Silent sessions time out after 10 seconds;
  WebSocket viewers that stop acknowledging frames time out after 3 seconds.

Remaining limits: capture still reads GPU→RAM before upload; not a DMA-BUF zero-copy
pipeline. No sound, clipboard synchronization, files, remote-window mode or rotated
monitor handling. Hardware decoder selection for WebRTC belongs to the browser.

## Overlay and verification

Top FPS and the first overlay FPS count video presentation callbacks (or decoded
frames on browsers without requestVideoFrameCallback). The separate Decode FPS
reports received/decoded frames, so decoding and presentation are not conflated.

- Capture: compositor request through frame-ready.
- Encode: conversion/upload/NVENC/packet copy.
- WebRTC Decode: interval totalDecodeTime / framesDecoded.
- WebRTC Jitter buf: interval jitterBufferDelay / jitterBufferEmittedCount.
- WebSocket Decode: packet receipt to decoder output; includes scheduling.
- WebSocket Draw: canvas draw call duration, not physical screen scanout.
- RTT: application ping (adaptation uses ICE RTT when exposed).
- Loss: interval lost / (received + lost) RTP packets.
- Bitrate: current encoder target.
- Decoder: actual implementation if browser exposes it; otherwise "browser video".
- Efficiency hint: MediaCapabilities estimate, **not proof** of active hardware decoding.
- Frame→ACK is shown only for WebSocket. It is not comparable to the individual
  WebRTC pipeline counters and is not input-to-photon latency.

Hardware tests on this RTX 4090, 4K60 encoder: CPU conversion median ~16 ms,
CUDA median ~5.5–5.8 ms. Live desktop WebRTC 4K decoded ~59–60 fps with encode
~6–7 ms, decode ~3 ms and jitter buffer ~6–9 ms after rate-control correction.
Presentation FPS depends on the local browser/compositor (roughly 49–57 in one sample).
These are desktop LAN/loopback measurements, not results on the user's Wi-Fi tablet.

    cargo test
    cargo test -- --ignored --nocapture  # requires NVIDIA + ffmpeg
    cargo clippy --all-targets -- -D warnings
    node --check static/app.js

Tests cover feedback thresholds/caps, bounded ACK flow, GPU color order, inversion,
scaling and FFmpeg decode roundtrip, runtime bitrate changes, repeated GPU encoder
creation/teardown. Live browser checks cover UDP negotiation, decode, adaptive
bitrate/resolution changes, transport switching and input.

Implementation references:
- [FFmpeg NVENC rate control](https://github.com/FFmpeg/FFmpeg/blob/n8.0/libavcodec/nvenc.c)
- [WebRTC stats definitions](https://www.w3.org/TR/webrtc-stats/)
- [Media Capabilities](https://www.w3.org/TR/media-capabilities/)
- [webrtc-rs examples](https://github.com/webrtc-rs/webrtc/tree/v0.17.1/examples)

## This machine

URL: https://192.168.8.150:8085
Start: ~/.local/share/hypr2web/start.sh
Stop: systemctl --user stop hypr2web
Logs: journalctl --user -u hypr2web -f

Persistent systemd user service, enabled for the graphical session:
~/.config/systemd/user/hypr2web.service. Restart: systemctl --user restart hypr2web.
The service uses target/release/hypr2web and reuses the trial TLS
certificate/key in ~/.local/share/rdp2web-hypr; renew/trust it as needed.
The RDP stack remains on port 8081.

## Attribution

MIT. Includes virtual-keyboard protocol/bindings from hypr-rdp and adapts the FFmpeg
NVENC bridge developed in https://github.com/DerpyCrabs/hypr-rdp.
Original license attribution is preserved in LICENSE and protocol XML.

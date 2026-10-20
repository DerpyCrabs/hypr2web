# hypr2web

Mirror and control an existing Hyprland monitor in a browser, without RDP.
Rust + Wayland screencopy → H.264 NVENC → HTTPS WebSocket → WebCodecs.
Input uses Wayland virtual keyboard and pointer independently of video.

## Build and run

Requires Rust, pkg-config, a C compiler, FFmpeg development libraries
(libavcodec, libavutil, libswscale), libxkbcommon, and an NVIDIA driver with NVENC.
On Arch these are provided by base-devel, rust, ffmpeg, libxkbcommon and the NVIDIA driver.

    cargo build --release
    ./target/release/hypr2web --bind 127.0.0.1:8082 --output HDMI-A-1 --cert ui.crt --key ui.key

Run inside the Hyprland session. For tablet access bind to the computer's LAN IP.
Use a certificate trusted by the tablet; WebCodecs requires a secure context.
The browser must support H.264 WebCodecs (checked on desktop Chromium).

This prototype has **no authentication** and permits one connected controller.
Anyone who can reach the listener can view/control the desktop. Intended for a
trusted LAN only. A second connection is rejected until the first disconnects.
It captures the existing monitor, including its cursor, rather than a new session.

## Interface

The panel follows rdp2web: it hides above the screen during a connection.
Hover/tap the strip at the top to reveal controls. Resolution changes reconnect.
Icons: reconnect, disconnect, release input, keyboard, statistics, settings, fullscreen.
Statistics are hidden by default; toggle the graph icon or open /?stats=1.
Settings contain FPS, bitrate and keyboard layout (EN/RU assume these are compositor
layout groups 0/1). The on-screen keyboard uses physical key positions.
Shift/Ctrl/Alt/Super buttons latch until toggled or input is released.
The regular keyboard is captured while the video canvas has focus.

## Latency design and limits

- At most two encoded frames awaiting presentation ACK. Slow clients pause capture.
- ACK is sent after drawing. Decoded frames may be superseded; encoded delta frames
  are never dropped from the decoder's reference chain.
- NVENC p1/ultra-low-latency, no B frames or lookahead.
- One fused CPU scale/color conversion, then GPU H.264 encoding.
- Independent input thread, bounded input and outgoing queues.
- Held input is released on disconnect. Unresponsive viewers time out after 3 s.

Capture still uses shared memory with a GPU→CPU readback. Scaling/color conversion
still uses CPU. This is not a zero-copy DMA-BUF pipeline. TCP can stall under Wi-Fi
packet loss; WebRTC would be a separate next step. No sound, file transfer, remote
window mode, clipboard synchronization or monitor rotation handling yet.

Overlay:
- Capture: request through compositor frame-ready.
- Encode: CPU scale/color conversion + upload + NVENC + packet copy.
- Draw: browser receipt through decoder and canvas draw.
- RTT: application ping roundtrip.
- Frame→ACK: server capture start through browser draw and ACK return; not
  input-to-photon latency and not physical display scanout.

Initial desktop loopback test (4090, 4K capture downscaled to 1080p60):
53–57 rendered fps over a five-second sample, frame→ACK 14–23 ms.
These are not tablet/Wi-Fi measurements or a controlled comparison with RDP.
Tests also covered resolution reconnect, pointer positioning, keyboard press/release
and modifier reset. UI checked through T3 Browser using a loopback TLS proxy.

    cargo test
    cargo test -- --ignored    # actual NVIDIA encoder required
    cargo clippy --all-targets -- -D warnings

## This machine

Running at https://192.168.8.150:8082.
Start: ~/.local/share/hypr2web/start.sh
Stop: systemctl --user stop hypr2web
Logs: journalctl --user -u hypr2web -f

The transient user service is not enabled at login. Start script currently reuses
the trial TLS certificate/key in ~/.local/share/rdp2web-hypr; renew/trust it as needed.
The existing RDP stack remains on port 8081.

## Attribution

MIT. Includes the virtual-keyboard protocol and bindings from hypr-rdp,
and adapts the FFmpeg NVENC bridge developed in https://github.com/DerpyCrabs/hypr-rdp.
Original license attribution is preserved in LICENSE and protocol XML.

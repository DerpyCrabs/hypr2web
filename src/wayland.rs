use crate::virtual_keyboard::{ZwpVirtualKeyboardManagerV1, ZwpVirtualKeyboardV1};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::HashMap,
    fs::File,
    io::Write,
    os::{
        fd::{AsFd, AsRawFd, FromRawFd},
        unix::fs::FileExt,
    },
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use wayland_client::protocol::{
    wl_buffer, wl_keyboard, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::{
    screencopy::v1::client::{zwlr_screencopy_frame_v1 as cf, zwlr_screencopy_manager_v1 as cm},
    virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1 as pm, zwlr_virtual_pointer_v1 as vp,
    },
};

#[derive(Default)]
pub struct State {
    pub shm: Option<wl_shm::WlShm>,
    pub capture: Option<cm::ZwlrScreencopyManagerV1>,
    pub pointer: Option<pm::ZwlrVirtualPointerManagerV1>,
    pub keyboard: Option<ZwpVirtualKeyboardManagerV1>,
    pub seat: Option<wl_seat::WlSeat>,
    pub outputs: HashMap<u32, (wl_output::WlOutput, String)>,
    pub params: Option<(u32, u32, u32, wl_shm::Format)>,
    pub buffer_done: bool,
    pub ready: bool,
    pub failed: bool,
    pub inverted: bool,
    pub keymap: Option<String>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        r: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = e
        {
            match interface.as_str() {
                "wl_shm" => s.shm = Some(r.bind(name, 1, q, ())),
                "wl_output" => {
                    s.outputs
                        .insert(name, (r.bind(name, version.min(4), q, name), String::new()));
                }
                "wl_seat" => s.seat = Some(r.bind(name, version.min(7), q, ())),
                "zwlr_screencopy_manager_v1" => {
                    s.capture = Some(r.bind(name, version.min(3), q, ()))
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    s.pointer = Some(r.bind(name, version.min(2), q, ()))
                }
                "zwp_virtual_keyboard_manager_v1" => s.keyboard = Some(r.bind(name, 1, q, ())),
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        s: &mut Self,
        _: &wl_output::WlOutput,
        e: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = e
            && let Some(output) = s.outputs.get_mut(id)
        {
            output.1 = name;
        }
    }
}
impl Dispatch<cf::ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &cf::ZwlrScreencopyFrameV1,
        e: cf::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            cf::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                if matches!(
                    format,
                    wl_shm::Format::Argb8888
                        | wl_shm::Format::Xrgb8888
                        | wl_shm::Format::Abgr8888
                        | wl_shm::Format::Xbgr8888
                ) {
                    s.params = Some((width, height, stride, format));
                }
            }
            cf::Event::BufferDone => s.buffer_done = true,
            cf::Event::Ready { .. } => s.ready = true,
            cf::Event::Failed => s.failed = true,
            cf::Event::Flags {
                flags: WEnum::Value(flags),
            } => s.inverted = flags.contains(cf::Flags::YInvert),
            _ => {}
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        s: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        e: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap {
            format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
            fd,
            size,
        } = e
        {
            if size > 4 * 1024 * 1024 {
                return;
            }
            let mut bytes = vec![0; size as usize];
            if File::from(fd).read_exact_at(&mut bytes, 0).is_ok()
                && let Ok(text) = String::from_utf8(bytes)
                && !text.trim_end_matches('\0').is_empty()
            {
                s.keymap = Some(text.trim_end_matches('\0').to_owned());
            }
        }
    }
}
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore cm::ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore pm::ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore vp::ZwlrVirtualPointerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardV1);

pub struct Wayland {
    pub conn: Connection,
    pub queue: EventQueue<State>,
    pub state: State,
    pub qh: QueueHandle<State>,
    pub output: wl_output::WlOutput,
}
impl Wayland {
    pub fn new(name: &str) -> Result<Self> {
        let conn = Connection::connect_to_env().context("connect to Hyprland")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let mut state = State::default();
        conn.display().get_registry(&qh, ());
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        let output = state
            .outputs
            .values()
            .find(|(_, n)| n == name)
            .map(|(o, _)| o.clone())
            .with_context(|| format!("output {name} not found"))?;
        Ok(Self {
            conn,
            queue,
            state,
            qh,
            output,
        })
    }
    pub fn pump(&mut self, timeout_ms: i32) -> Result<()> {
        self.queue.dispatch_pending(&mut self.state)?;
        self.conn.flush()?;
        if let Some(guard) = self.queue.prepare_read() {
            let mut fd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll receives one valid initialized pollfd.
            let ready = unsafe { libc::poll(&mut fd, 1, timeout_ms) };
            if ready > 0 {
                guard.read()?;
            } else if ready < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }
    pub fn wait(&mut self, stop: &AtomicBool, done: impl Fn(&State) -> bool) -> Result<()> {
        let start = Instant::now();
        while !done(&self.state) {
            ensure!(!stop.load(Ordering::Relaxed), "session closed");
            ensure!(!self.state.failed, "compositor capture failed");
            ensure!(
                start.elapsed() < Duration::from_secs(2),
                "Wayland capture timed out"
            );
            self.pump(5)?;
        }
        Ok(())
    }
}
pub fn memfd(size: usize) -> Result<File> {
    // SAFETY: valid NUL-terminated name; returned FD becomes exclusively File-owned.
    let fd = unsafe { libc::memfd_create(c"hypr2web".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(size as u64)?;
    Ok(file)
}
pub fn keymap_file(text: &str) -> Result<File> {
    let mut f = memfd(text.len() + 1)?;
    f.write_all(text.as_bytes())?;
    f.write_all(&[0])?;
    Ok(f)
}

pub fn capture(
    config: crate::StreamConfig,
    output: String,
    shared: Arc<crate::Session>,
    tx: tokio::sync::mpsc::Sender<crate::Outgoing>,
) -> Result<()> {
    let mut wl = Wayland::new(&output)?;
    let manager = wl
        .state
        .capture
        .clone()
        .context("Hyprland does not expose wlr-screencopy")?;
    let shm = wl.state.shm.clone().context("wl_shm is unavailable")?;
    let probe = manager.capture_output(1, &wl.output, &wl.qh, ());
    wl.wait(&shared.stop, |s| s.buffer_done)?;
    let (w, h, stride, format) = wl
        .state
        .params
        .context("no supported screen pixel format")?;
    probe.destroy();
    let mut size = crate::encoder::fit_size(w, h, config.width);
    let rgba = matches!(format, wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888);
    let mut encoder = Some(crate::encoder::Encoder::new(
        (w, h),
        size,
        rgba,
        config.fps,
        config.bitrate,
        config.gpu,
    )?);
    let bytes = (stride as usize)
        .checked_mul(h as usize)
        .context("capture size overflow")?;
    ensure!(bytes <= 512 * 1024 * 1024, "capture buffer exceeds 512 MiB");
    let file = memfd(bytes)?;
    // SAFETY: File length is bytes; Wayland writes only while a copy is in progress.
    // Encoding reads the mapping only after the compositor's Ready event.
    let mapping = unsafe { memmap2::MmapOptions::new().len(bytes).map(&file)? };
    let pool = shm.create_pool(file.as_fd(), i32::try_from(bytes)?, &wl.qh, ());
    let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, &wl.qh, ());
    let _=tx.blocking_send(crate::Outgoing::Json(serde_json::json!({"type":"stream","width":size.0,"height":size.1,"sourceWidth":w,"sourceHeight":h,"output":output,"fps":config.fps,"bitrate":config.bitrate,"encoder":"NVENC"})));
    tracing::info!(
        ?size,
        source_width = w,
        source_height = h,
        fps = config.fps,
        "direct NVENC stream started"
    );
    let mut period = Duration::from_secs_f64(1. / config.fps as f64);
    let mut current_bitrate = config.bitrate;
    let mut current_fps = config.fps;
    let mut next = Instant::now();
    let mut seq = 0u32;
    while !shared.stop.load(Ordering::Relaxed) {
        if config.transport == crate::Transport::Ws && shared.flow.lock().unwrap().stalled() {
            bail!("browser stopped acknowledging displayed frames");
        }
        if Instant::now() < next
            || (config.transport == crate::Transport::Ws
                && !shared.flow.lock().unwrap().available())
            || (config.transport == crate::Transport::Webrtc
                && !shared.rtc_ready.load(Ordering::Relaxed))
        {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        let (bitrate, width, fps) = {
            let a = shared.adaptive.lock().unwrap();
            (a.bitrate, a.width, a.fps)
        };
        let new_size = crate::encoder::fit_size(w, h, width);
        if new_size != size || fps != current_fps {
            size = new_size;
            current_fps = fps;
            drop(encoder.take());
            encoder = Some(crate::encoder::Encoder::new(
                (w, h),
                size,
                rgba,
                fps,
                bitrate,
                config.gpu,
            )?);
            period = Duration::from_secs_f64(1. / fps as f64);
            shared.keyframe.store(true, Ordering::Relaxed);
            let _=tx.blocking_send(crate::Outgoing::Json(serde_json::json!({"type":"stream","width":size.0,"height":size.1,"fps":fps,"bitrate":bitrate,"encoder":"NVENC","gpu":config.gpu})));
        }
        if bitrate != current_bitrate {
            encoder.as_mut().unwrap().set_bitrate(bitrate);
            current_bitrate = bitrate;
        }
        wl.state.params = None;
        wl.state.buffer_done = false;
        wl.state.ready = false;
        wl.state.failed = false;
        wl.state.inverted = false;
        let captured = Instant::now();
        let frame = manager.capture_output(1, &wl.output, &wl.qh, ());
        wl.wait(&shared.stop, |s| s.buffer_done)?;
        ensure!(
            wl.state.params == Some((w, h, stride, format)),
            "monitor geometry changed; reconnect to refresh capture"
        );
        frame.copy(&buffer);
        wl.conn.flush()?;
        wl.wait(&shared.stop, |s| s.ready)?;
        frame.destroy();
        let capture_us = captured.elapsed().as_micros().min(u32::MAX as u128) as u32;
        let encoded = Instant::now();
        let idr = shared.keyframe.swap(false, Ordering::Relaxed) || seq == 0;
        let data =
            encoder
                .as_mut()
                .unwrap()
                .encode(&mapping, stride as usize, wl.state.inverted, idr)?;
        let encode_us = encoded.elapsed().as_micros().min(u32::MAX as u128) as u32;
        seq = seq
            .checked_add(1)
            .context("frame sequence exhausted; reconnect")?;
        let mut packet = Vec::with_capacity(24 + data.len());
        for n in [0x31425748, seq, capture_us, encode_us, size.0, size.1] {
            packet.extend_from_slice(&n.to_le_bytes());
        }
        packet.extend_from_slice(&data);
        *shared.timing.lock().unwrap() = (capture_us as f64 / 1000., encode_us as f64 / 1000.);
        if config.transport == crate::Transport::Ws {
            shared.flow.lock().unwrap().sent(seq, captured);
        }
        if tx
            .blocking_send(crate::Outgoing::Frame(packet.into()))
            .is_err()
        {
            break;
        }
        next = (next + period).max(Instant::now());
    }
    buffer.destroy();
    pool.destroy();
    Ok(())
}

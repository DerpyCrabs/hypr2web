use crate::{
    Session,
    wayland::{Wayland, keymap_file},
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::HashSet,
    os::fd::AsFd,
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use xkbcommon::xkb;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    Pointer { x: f64, y: f64 },
    Button { button: u8, down: bool },
    Wheel { delta: f64 },
    Key { code: u32, down: bool },
    Release,
    Layout { group: u32 },
}
pub fn run(output: String, session: Arc<Session>, rx: mpsc::Receiver<Input>) -> Result<()> {
    let mut wl = Wayland::new(&output)?;
    let seat = wl.state.seat.clone().context("wl_seat unavailable")?;
    let physical_keyboard = seat.get_keyboard(&wl.qh, ());
    let deadline = Instant::now() + Duration::from_secs(2);
    while wl.state.keymap.is_none() && Instant::now() < deadline {
        wl.pump(10)?;
    }
    let text = wl
        .state
        .keymap
        .clone()
        .context("compositor keyboard keymap unavailable")?;
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_string(
        &context,
        text.clone(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .context("parse compositor keymap")?;
    let mut state = xkb::State::new(&keymap);
    if let Some(group) = initial_group(&keymap) {
        state.update_mask(0, 0, 0, 0, 0, group);
    }
    let keyboard = wl
        .state
        .keyboard
        .as_ref()
        .context("virtual keyboard protocol unavailable")?
        .create_virtual_keyboard(&seat, &wl.qh, ());
    let fd = keymap_file(&text)?;
    keyboard.keymap(1, fd.as_fd(), (text.len() + 1) as u32);
    let pointer = wl
        .state
        .pointer
        .as_ref()
        .context("virtual pointer protocol unavailable")?
        .create_virtual_pointer_with_output(Some(&seat), Some(&wl.output), &wl.qh, ());
    let mut keys = HashSet::new();
    let mut buttons = HashSet::new();
    let clock = Instant::now();
    let modifiers = |s: &xkb::State| {
        keyboard.modifiers(
            s.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            s.serialize_mods(xkb::STATE_MODS_LATCHED),
            s.serialize_mods(xkb::STATE_MODS_LOCKED),
            s.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        )
    };
    modifiers(&state);
    wl.conn.flush()?;
    let result = (|| -> Result<()> {
        while !session.stop.load(Ordering::Relaxed) {
            let event = match rx.recv_timeout(Duration::from_millis(5)) {
                Ok(e) => e,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    wl.pump(0)?;
                    continue;
                }
                Err(_) => break,
            };
            let time = clock.elapsed().as_millis() as u32;
            match event {
                Input::Pointer { x, y } => {
                    if x.is_finite() && y.is_finite() {
                        pointer.motion_absolute(
                            time,
                            (x.clamp(0., 1.) * 1_000_000.) as u32,
                            (y.clamp(0., 1.) * 1_000_000.) as u32,
                            1_000_000,
                            1_000_000,
                        );
                        pointer.frame();
                    }
                }
                Input::Button { button, down } => {
                    if let Some(code) = button_code(button) {
                        let changed = if down {
                            buttons.insert(code)
                        } else {
                            buttons.remove(&code)
                        };
                        if changed {
                            pointer.button(
                                time,
                                code,
                                if down {
                                    ButtonState::Pressed
                                } else {
                                    ButtonState::Released
                                },
                            );
                            pointer.frame();
                        }
                    }
                }
                Input::Wheel { delta } => {
                    if delta.is_finite() {
                        pointer.axis_source(AxisSource::Wheel);
                        pointer.axis(time, Axis::VerticalScroll, delta.clamp(-240., 240.));
                        pointer.frame();
                    }
                }
                Input::Key { code, down } => {
                    if (1..=255).contains(&code) {
                        let changed = if down {
                            keys.insert(code)
                        } else {
                            keys.remove(&code)
                        };
                        if changed {
                            keyboard.key(time, code, u32::from(down));
                            state.update_key(
                                xkb::Keycode::new(code + 8),
                                if down {
                                    xkb::KeyDirection::Down
                                } else {
                                    xkb::KeyDirection::Up
                                },
                            );
                            modifiers(&state);
                        }
                    }
                }
                Input::Layout { group } => {
                    if group < keymap.num_layouts() {
                        state.update_mask(
                            state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                            0,
                            state.serialize_mods(xkb::STATE_MODS_LOCKED),
                            0,
                            0,
                            group,
                        );
                        modifiers(&state);
                    }
                }
                Input::Release => {
                    for code in keys.drain() {
                        keyboard.key(time, code, 0);
                        state.update_key(xkb::Keycode::new(code + 8), xkb::KeyDirection::Up);
                    }
                    for code in buttons.drain() {
                        pointer.button(time, code, ButtonState::Released);
                    }
                    pointer.frame();
                    modifiers(&state);
                }
            }
            wl.conn.flush()?;
        }
        Ok(())
    })();
    // Always release held input on disconnect/error before destroying the virtual devices.
    let time = clock.elapsed().as_millis() as u32;
    for code in keys {
        keyboard.key(time, code, 0);
        state.update_key(xkb::Keycode::new(code + 8), xkb::KeyDirection::Up);
    }
    for code in buttons {
        pointer.button(time, code, ButtonState::Released);
    }
    pointer.frame();
    modifiers(&state);
    let _ = wl.conn.flush();
    keyboard.destroy();
    pointer.destroy();
    physical_keyboard.release();
    let _ = wl.conn.flush();
    result
}
fn initial_group(keymap: &xkb::Keymap) -> Option<u32> {
    let output = std::process::Command::new("hyprctl")
        .args(["devices", "-j"])
        .output()
        .ok()?;
    let data: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let keyboards = data["keyboards"].as_array()?;
    let physical = keyboards
        .iter()
        .filter(|k| {
            !k["name"]
                .as_str()
                .unwrap_or("")
                .starts_with("hl-virtual-keyboard")
        })
        .collect::<Vec<_>>();
    let k = physical
        .iter()
        .find(|k| k["main"].as_bool() == Some(true))
        .or(physical.first())?;
    let name = k["active_keymap"].as_str()?;
    (0..keymap.num_layouts()).find(|&group| keymap.layout_get_name(group) == name)
}
fn button_code(button: u8) -> Option<u32> {
    match button {
        0 => Some(0x110),
        1 => Some(0x112),
        2 => Some(0x111),
        _ => None,
    }
}
pub fn validate_size(n: usize) -> Result<()> {
    ensure!(n <= 16 * 1024, "input message too large");
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_dom_mouse_buttons() {
        assert_eq!(button_code(0), Some(0x110));
        assert_eq!(button_code(2), Some(0x111));
        assert_eq!(button_code(9), None);
    }
}

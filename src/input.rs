use crate::{
    Session,
    wayland::{Wayland, keymap_file},
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
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
    Text { text: String },
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
    let text_keys = text_key_codes(&keymap);
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
    let text_keyboard =
        wl.state
            .keyboard
            .as_ref()
            .unwrap()
            .create_virtual_keyboard(&seat, &wl.qh, ());
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
                Input::Text { text } => {
                    for ch in text.chars().filter(|c| !c.is_control()) {
                        if let Some(&(code, mods, group)) = text_keys.get(&ch) {
                            // Use the same evdev code as a physical keyboard, including in games.
                            keyboard.modifiers(mods, 0, 0, group);
                            keyboard.key(time, code, 1);
                            keyboard.key(time, code, 0);
                            modifiers(&state);
                        } else {
                            // Characters absent from the layout use KEY_UNKNOWN, never Esc/WASD.
                            let map = unicode_keymap(ch);
                            let fd = keymap_file(&map)?;
                            text_keyboard.keymap(1, fd.as_fd(), (map.len() + 1) as u32);
                            text_keyboard.modifiers(0, 0, 0, 0);
                            text_keyboard.key(time, 240, 1);
                            text_keyboard.key(time, 240, 0);
                        }
                        wl.conn.flush()?;
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
    text_keyboard.destroy();
    keyboard.destroy();
    pointer.destroy();
    physical_keyboard.release();
    let _ = wl.conn.flush();
    result
}
fn text_key_codes(keymap: &xkb::Keymap) -> HashMap<char, (u32, u32, u32)> {
    let mut result = HashMap::new();
    let mut state = xkb::State::new(keymap);
    let mask = |name: &str| 1u32.checked_shl(keymap.mod_get_index(name)).unwrap_or(0);
    let shift = mask("Shift");
    let altgr = mask("Mod5");
    for group in 0..keymap.num_layouts() {
        for mods in [0, shift, altgr, shift | altgr] {
            state.update_mask(mods, 0, 0, 0, 0, group);
            for code in 1..=255 {
                let text = state.key_get_utf8(xkb::Keycode::new(code + 8));
                let mut chars = text.chars();
                if let Some(ch) = chars.next()
                    && chars.next().is_none()
                    && !ch.is_control()
                {
                    result.entry(ch).or_insert((code, mods, group));
                }
            }
        }
    }
    result
}
fn unicode_keymap(ch: char) -> String {
    format!(
        r#"xkb_keymap {{
        xkb_keycodes "text" {{ minimum = 8; maximum = 255; <TEXT> = 248; }};
        xkb_types "text" {{ type "ONE_LEVEL" {{ modifiers = None; map[None] = Level1; }}; }};
        xkb_compatibility "text" {{}};
        xkb_symbols "text" {{ key <TEXT> {{ [ U{:04X} ] }}; }};
    }};"#,
        ch as u32
    )
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
    fn text_uses_physical_key_codes() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let map = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            "us,ru",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let keys = text_key_codes(&map);
        for (ch, code) in [
            ('w', 17),
            ('a', 30),
            ('s', 31),
            ('d', 32),
            ('D', 32),
            ('1', 2),
            (' ', 57),
            ('в', 32),
        ] {
            assert_eq!(keys[&ch].0, code, "{ch} must use its physical key");
            let (_, mods, group) = keys[&ch];
            let mut state = xkb::State::new(&map);
            state.update_mask(mods, 0, 0, 0, 0, group);
            assert_eq!(
                state.key_get_utf8(xkb::Keycode::new(code + 8)),
                ch.to_string()
            );
        }
        assert!(
            keys.values().all(|&(code, _, _)| code != 1),
            "text must never send Escape"
        );
    }
    #[test]
    fn unicode_text_keymap_roundtrip() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        for ch in "Hello, Привет! 😀é".chars() {
            let map = xkb::Keymap::new_from_string(
                &context,
                unicode_keymap(ch),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .unwrap();
            let state = xkb::State::new(&map);
            assert_eq!(state.key_get_utf8(xkb::Keycode::new(248)), ch.to_string());
            assert_eq!(state.key_get_utf8(xkb::Keycode::new(9)), "");
        }
    }
    #[test]
    fn maps_dom_mouse_buttons() {
        assert_eq!(button_code(0), Some(0x110));
        assert_eq!(button_code(2), Some(0x111));
        assert_eq!(button_code(9), None);
    }
}

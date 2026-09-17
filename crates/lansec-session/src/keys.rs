use winit::event::{ElementState, MouseButton, MouseScrollDelta};
use winit::keyboard::{KeyCode, PhysicalKey};

use lansec_protocol::InputEvent;

pub fn mouse_button(button: MouseButton, down: bool) -> InputEvent {
    let button = match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
        MouseButton::Other(n) => n as u8,
    };
    InputEvent::MouseButton { button, down }
}

pub fn mouse_wheel(delta: MouseScrollDelta) -> Option<InputEvent> {
    let (dx, dy) = match delta {
        MouseScrollDelta::LineDelta(x, y) => (x as i16, y as i16),
        MouseScrollDelta::PixelDelta(p) => ((p.x as i16) / 16, (p.y as i16) / 16),
    };
    if dx == 0 && dy == 0 {
        None
    } else {
        Some(InputEvent::MouseWheel { dx, dy })
    }
}

pub fn key(physical: PhysicalKey, state: ElementState) -> Option<InputEvent> {
    let PhysicalKey::Code(code) = physical else {
        return None;
    };
    let vk = keycode_to_vk(code)?;
    Some(InputEvent::Key {
        vk,
        down: state == ElementState::Pressed,
        scancode: 0,
    })
}

/// Map winit physical keys to Windows VK codes (wire format).
/// Returns `None` for keys we do not support — never send VK=0 (was typed as 'A' on Mac).
fn keycode_to_vk(code: KeyCode) -> Option<u16> {
    Some(match code {
        KeyCode::Escape => 0x1B,
        KeyCode::Enter | KeyCode::NumpadEnter => 0x0D,
        KeyCode::Tab => 0x09,
        KeyCode::Backspace => 0x08,
        KeyCode::Delete => 0x2E,
        KeyCode::Space => 0x20,
        KeyCode::ShiftLeft => 0xA0,
        KeyCode::ShiftRight => 0xA1,
        KeyCode::ControlLeft => 0xA2,
        KeyCode::ControlRight => 0xA3,
        KeyCode::AltLeft => 0xA4,
        KeyCode::AltRight => 0xA5,
        // Win/Super → LWIN/RWIN; Mac host maps these to Command.
        KeyCode::SuperLeft | KeyCode::Meta => 0x5B,
        KeyCode::SuperRight => 0x5C,
        KeyCode::CapsLock => 0x14,
        KeyCode::ArrowLeft => 0x25,
        KeyCode::ArrowUp => 0x26,
        KeyCode::ArrowRight => 0x27,
        KeyCode::ArrowDown => 0x28,
        KeyCode::Home => 0x24,
        KeyCode::End => 0x23,
        KeyCode::PageUp => 0x21,
        KeyCode::PageDown => 0x22,
        KeyCode::Digit0 => 0x30,
        KeyCode::Digit1 => 0x31,
        KeyCode::Digit2 => 0x32,
        KeyCode::Digit3 => 0x33,
        KeyCode::Digit4 => 0x34,
        KeyCode::Digit5 => 0x35,
        KeyCode::Digit6 => 0x36,
        KeyCode::Digit7 => 0x37,
        KeyCode::Digit8 => 0x38,
        KeyCode::Digit9 => 0x39,
        KeyCode::KeyA => 0x41,
        KeyCode::KeyB => 0x42,
        KeyCode::KeyC => 0x43,
        KeyCode::KeyD => 0x44,
        KeyCode::KeyE => 0x45,
        KeyCode::KeyF => 0x46,
        KeyCode::KeyG => 0x47,
        KeyCode::KeyH => 0x48,
        KeyCode::KeyI => 0x49,
        KeyCode::KeyJ => 0x4A,
        KeyCode::KeyK => 0x4B,
        KeyCode::KeyL => 0x4C,
        KeyCode::KeyM => 0x4D,
        KeyCode::KeyN => 0x4E,
        KeyCode::KeyO => 0x4F,
        KeyCode::KeyP => 0x50,
        KeyCode::KeyQ => 0x51,
        KeyCode::KeyR => 0x52,
        KeyCode::KeyS => 0x53,
        KeyCode::KeyT => 0x54,
        KeyCode::KeyU => 0x55,
        KeyCode::KeyV => 0x56,
        KeyCode::KeyW => 0x57,
        KeyCode::KeyX => 0x58,
        KeyCode::KeyY => 0x59,
        KeyCode::KeyZ => 0x5A,
        KeyCode::F1 => 0x70,
        KeyCode::F2 => 0x71,
        KeyCode::F3 => 0x72,
        KeyCode::F4 => 0x73,
        KeyCode::F5 => 0x74,
        KeyCode::F6 => 0x75,
        KeyCode::F7 => 0x76,
        KeyCode::F8 => 0x77,
        KeyCode::F9 => 0x78,
        KeyCode::F10 => 0x79,
        KeyCode::F11 => 0x7A,
        KeyCode::F12 => 0x7B,
        // Punctuation (US ANSI OEM VKs)
        KeyCode::Minus => 0xBD,
        KeyCode::Equal => 0xBB,
        KeyCode::BracketLeft => 0xDB,
        KeyCode::BracketRight => 0xDD,
        KeyCode::Backslash => 0xDC,
        KeyCode::Semicolon => 0xBA,
        KeyCode::Quote => 0xDE,
        KeyCode::Comma => 0xBC,
        KeyCode::Period => 0xBE,
        KeyCode::Slash => 0xBF,
        KeyCode::Backquote => 0xC0,
        // Numpad
        KeyCode::Numpad0 => 0x60,
        KeyCode::Numpad1 => 0x61,
        KeyCode::Numpad2 => 0x62,
        KeyCode::Numpad3 => 0x63,
        KeyCode::Numpad4 => 0x64,
        KeyCode::Numpad5 => 0x65,
        KeyCode::Numpad6 => 0x66,
        KeyCode::Numpad7 => 0x67,
        KeyCode::Numpad8 => 0x68,
        KeyCode::Numpad9 => 0x69,
        KeyCode::NumpadMultiply => 0x6A,
        KeyCode::NumpadAdd => 0x6B,
        KeyCode::NumpadSubtract => 0x6D,
        KeyCode::NumpadDecimal => 0x6E,
        KeyCode::NumpadDivide => 0x6F,
        KeyCode::NumLock => 0x90,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::keycode_to_vk;
    use winit::keyboard::KeyCode;

    #[test]
    fn maps_punctuation() {
        assert_eq!(keycode_to_vk(KeyCode::Period), Some(0xBE));
        assert_eq!(keycode_to_vk(KeyCode::Comma), Some(0xBC));
        assert_eq!(keycode_to_vk(KeyCode::Minus), Some(0xBD));
        assert_eq!(keycode_to_vk(KeyCode::Slash), Some(0xBF));
    }
}

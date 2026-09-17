use lansec_protocol::InputEvent;

use crate::InputError;

/// Tracks which mouse buttons are down so moves become drag events on macOS.
#[derive(Debug, Clone, Default)]
pub struct MouseButtons {
    down: [bool; 5],
    last_x: u16,
    last_y: u16,
    have_pos: bool,
}

impl MouseButtons {
    pub fn any_down(&self) -> bool {
        self.down.iter().any(|&d| d)
    }

    pub fn drag_button(&self) -> i32 {
        if self.down[0] {
            return 0;
        }
        if self.down[1] {
            return 1;
        }
        if self.down[2] {
            return 2;
        }
        -1
    }

    fn set_button(&mut self, button: u8, down: bool) {
        if (button as usize) < self.down.len() {
            self.down[button as usize] = down;
        }
    }

    fn note_pos(&mut self, x: u16, y: u16) {
        self.last_x = x;
        self.last_y = y;
        self.have_pos = true;
    }

    fn click_pos(&self) -> (u16, u16) {
        if self.have_pos {
            (self.last_x, self.last_y)
        } else {
            unsafe {
                extern "C" {
                    fn lansec_cg_cursor_pos(x: *mut u16, y: *mut u16) -> i32;
                }
                let mut x = 0u16;
                let mut y = 0u16;
                if lansec_cg_cursor_pos(&mut x, &mut y) != 0 {
                    (x, y)
                } else {
                    (0, 0)
                }
            }
        }
    }
}

pub fn inject(ev: &InputEvent) -> Result<(), InputError> {
    inject_with_buttons(ev, &mut MouseButtons::default())
}

pub fn inject_with_buttons(ev: &InputEvent, buttons: &mut MouseButtons) -> Result<(), InputError> {
    match ev {
        InputEvent::MouseMoveAbs { x, y, .. } => {
            buttons.note_pos(*x, *y);
            unsafe { cg_mouse_abs(*x, *y, buttons.drag_button()) }
        }
        InputEvent::MouseMoveRel { dx, dy } => {
            let (x, y) = buttons.click_pos();
            let nx = (x as i32 + *dx as i32).clamp(0, u16::MAX as i32) as u16;
            let ny = (y as i32 + *dy as i32).clamp(0, u16::MAX as i32) as u16;
            buttons.note_pos(nx, ny);
            unsafe { cg_mouse_abs(nx, ny, buttons.drag_button()) }
        }
        InputEvent::MouseButton { button, down } => {
            let (x, y) = buttons.click_pos();
            buttons.set_button(*button, *down);
            unsafe { cg_button(*button, *down, x, y) }
        }
        InputEvent::MouseWheel { dx, dy } => unsafe { cg_wheel(*dx, *dy) },
        InputEvent::Key { vk, down, .. } => {
            let Some(cg) = windows_vk_to_cg(*vk) else {
                // Unknown VK — do not inject CGKeyCode 0 (that types 'A').
                return Ok(());
            };
            unsafe { cg_key(cg, *down) }
        }
    }
}

/// Wire format uses Windows virtual-key codes; CGEvent wants Carbon `kVK_*` codes.
/// Returns `None` for unmapped keys so we never fall through to a wrong CGKeyCode.
fn windows_vk_to_cg(vk: u16) -> Option<u16> {
    // Carbon Events.h / HIToolbox virtual key codes (ANSI US positions).
    Some(match vk {
        // Letters A–Z
        0x41 => 0x00, // A
        0x53 => 0x01, // S
        0x44 => 0x02, // D
        0x46 => 0x03, // F
        0x48 => 0x04, // H
        0x47 => 0x05, // G
        0x5A => 0x06, // Z
        0x58 => 0x07, // X
        0x43 => 0x08, // C
        0x56 => 0x09, // V
        0x42 => 0x0B, // B
        0x51 => 0x0C, // Q
        0x57 => 0x0D, // W
        0x45 => 0x0E, // E
        0x52 => 0x0F, // R
        0x59 => 0x10, // Y
        0x54 => 0x11, // T
        0x31 => 0x12, // 1
        0x32 => 0x13, // 2
        0x33 => 0x14, // 3
        0x34 => 0x15, // 4
        0x36 => 0x16, // 6
        0x35 => 0x17, // 5
        0xBB => 0x18, // OEM_PLUS =
        0x39 => 0x19, // 9
        0x37 => 0x1A, // 7
        0xBD => 0x1B, // OEM_MINUS -
        0x38 => 0x1C, // 8
        0x30 => 0x1D, // 0
        0xDD => 0x1E, // OEM_6 ]
        0x4F => 0x1F, // O
        0x55 => 0x20, // U
        0xDB => 0x21, // OEM_4 [
        0x49 => 0x22, // I
        0x50 => 0x23, // P
        0x0D => 0x24, // Return
        0x4C => 0x25, // L
        0x4A => 0x26, // J
        0xDE => 0x27, // OEM_7 '
        0x4B => 0x28, // K
        0xBA => 0x29, // OEM_1 ;
        0xDC => 0x2A, // OEM_5 \
        0xBC => 0x2B, // OEM_COMMA ,
        0xBF => 0x2C, // OEM_2 /
        0x4E => 0x2D, // N
        0x4D => 0x2E, // M
        0xBE => 0x2F, // OEM_PERIOD .
        0x09 => 0x30, // Tab
        0x20 => 0x31, // Space
        0xC0 => 0x32, // OEM_3 `
        0x08 => 0x33, // Backspace (Mac Delete)
        0x1B => 0x35, // Escape
        // Modifiers — Win/Super maps to Command so Ctrl+C on Win client can be
        // remapped by the user; physical Ctrl/Alt still map to Control/Option.
        0x5B | 0x5C => 0x37, // LWIN/RWIN → Command (left; right also 0x36 if needed)
        0x10 | 0xA0 => 0x38, // Shift / LShift
        0xA1 => 0x3C,        // RShift
        0x14 => 0x39,        // CapsLock
        0x12 | 0xA4 => 0x3A, // Alt / LMenu → Option
        0xA5 => 0x3D,        // RMenu → Right Option
        0x11 | 0xA2 => 0x3B, // Control / LControl
        0xA3 => 0x3E,        // RControl
        // Navigation
        0x25 => 0x7B, // Left
        0x27 => 0x7C, // Right
        0x28 => 0x7D, // Down
        0x26 => 0x7E, // Up
        0x2E => 0x75, // DELETE → ForwardDelete
        0x24 => 0x73, // HOME
        0x23 => 0x77, // END
        0x21 => 0x74, // PRIOR PageUp
        0x22 => 0x79, // NEXT PageDown
        // F-keys (non-sequential on Mac)
        0x70 => 0x7A, // F1
        0x71 => 0x78, // F2
        0x72 => 0x63, // F3
        0x73 => 0x76, // F4
        0x74 => 0x60, // F5
        0x75 => 0x61, // F6
        0x76 => 0x62, // F7
        0x77 => 0x64, // F8
        0x78 => 0x65, // F9
        0x79 => 0x6D, // F10
        0x7A => 0x67, // F11
        0x7B => 0x6F, // F12
        // Numpad
        0x60 => 0x52, // Numpad0
        0x61 => 0x53, // Numpad1
        0x62 => 0x54, // Numpad2
        0x63 => 0x55, // Numpad3
        0x64 => 0x56, // Numpad4
        0x65 => 0x57, // Numpad5
        0x66 => 0x58, // Numpad6
        0x67 => 0x59, // Numpad7
        0x68 => 0x5B, // Numpad8
        0x69 => 0x5C, // Numpad9
        0x6E => 0x41, // Decimal
        0x6A => 0x43, // Multiply
        0x6B => 0x45, // Add
        0x6D => 0x4E, // Subtract
        0x6F => 0x4B, // Divide
        0x90 => 0x47, // NumLock → KeypadClear
        _ => return None,
    })
}

unsafe fn cg_mouse_abs(x: u16, y: u16, drag_button: i32) -> Result<(), InputError> {
    extern "C" {
        fn lansec_cg_mouse_abs(x: u16, y: u16, drag_button: i32) -> i32;
    }
    if lansec_cg_mouse_abs(x, y, drag_button) == 0 {
        Err(InputError::Message("CGEvent mouse abs failed".into()))
    } else {
        Ok(())
    }
}

unsafe fn cg_button(button: u8, down: bool, x: u16, y: u16) -> Result<(), InputError> {
    extern "C" {
        fn lansec_cg_button(button: u8, down: i32, x: u16, y: u16) -> i32;
    }
    if lansec_cg_button(button, i32::from(down), x, y) == 0 {
        Err(InputError::Message("CGEvent button failed".into()))
    } else {
        Ok(())
    }
}

unsafe fn cg_wheel(dx: i16, dy: i16) -> Result<(), InputError> {
    extern "C" {
        fn lansec_cg_wheel(dx: i16, dy: i16) -> i32;
    }
    if lansec_cg_wheel(dx, dy) == 0 {
        Err(InputError::Message("CGEvent wheel failed".into()))
    } else {
        Ok(())
    }
}

unsafe fn cg_key(vk: u16, down: bool) -> Result<(), InputError> {
    extern "C" {
        fn lansec_cg_key(vk: u16, down: i32) -> i32;
    }
    if lansec_cg_key(vk, i32::from(down)) == 0 {
        Err(InputError::Message("CGEvent key failed".into()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::windows_vk_to_cg;

    #[test]
    fn maps_punctuation_and_rejects_unknown() {
        assert_eq!(windows_vk_to_cg(0xBE), Some(0x2F)); // period
        assert_eq!(windows_vk_to_cg(0xBC), Some(0x2B)); // comma
        assert_eq!(windows_vk_to_cg(0xBD), Some(0x1B)); // minus
        assert_eq!(windows_vk_to_cg(0x41), Some(0x00)); // A
        assert_eq!(windows_vk_to_cg(0), None);
        assert_eq!(windows_vk_to_cg(0xFF), None);
    }
}

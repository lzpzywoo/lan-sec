use lansec_protocol::InputEvent;

use crate::InputError;

/// Tracks which mouse buttons are down so moves become drag events on macOS.
#[derive(Debug, Clone, Default)]
pub struct MouseButtons {
    down: [bool; 5],
}

impl MouseButtons {
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
}

pub fn inject(ev: &InputEvent) -> Result<(), InputError> {
    inject_with_buttons(ev, &mut MouseButtons::default())
}

pub fn inject_with_buttons(ev: &InputEvent, buttons: &mut MouseButtons) -> Result<(), InputError> {
    match ev {
        InputEvent::MouseMoveAbs { x, y, .. } => {
            unsafe { cg_mouse_abs(*x, *y, buttons.drag_button()) }
        }
        InputEvent::MouseMoveRel { dx, dy } => unsafe { cg_mouse_rel(*dx, *dy, buttons.drag_button()) },
        InputEvent::MouseButton { button, down } => {
            buttons.set_button(*button, *down);
            unsafe { cg_button(*button, *down) }
        }
        InputEvent::MouseWheel { dx, dy } => unsafe { cg_wheel(*dx, *dy) },
        InputEvent::Key { vk, down, .. } => unsafe { cg_key(windows_vk_to_cg(*vk), *down) },
    }
}

/// Wire format uses Windows virtual-key codes; CGEvent wants CGKeyCode.
fn windows_vk_to_cg(vk: u16) -> u16 {
    match vk {
        0x0D => 36,  // return
        0x08 => 51,  // delete
        0x09 => 48,  // tab
        0x1B => 53,  // escape
        0x20 => 49,  // space
        0x25 => 123, // left
        0x26 => 126, // up
        0x27 => 124, // right
        0x28 => 125, // down
        0x10 => 56,  // shift
        0x11 => 59,  // control
        0x12 => 58,  // option
        0x70..=0x7B => 122 + (vk - 0x70), // F1-F12-ish
        b if (b'A'..=b'Z').contains(&(b as u8)) => ansi_letter(b as u8),
        b if (b'0'..=b'9').contains(&(b as u8)) => ansi_digit(b as u8),
        other => other,
    }
}

fn ansi_letter(b: u8) -> u16 {
    match b {
        b'A' => 0,
        b'S' => 1,
        b'D' => 2,
        b'F' => 3,
        b'H' => 4,
        b'G' => 5,
        b'Z' => 6,
        b'X' => 7,
        b'C' => 8,
        b'V' => 9,
        b'B' => 11,
        b'Q' => 12,
        b'W' => 13,
        b'E' => 14,
        b'R' => 15,
        b'Y' => 16,
        b'T' => 17,
        b'O' => 31,
        b'U' => 32,
        b'I' => 34,
        b'P' => 35,
        b'L' => 37,
        b'J' => 38,
        b'K' => 40,
        b'N' => 45,
        b'M' => 46,
        _ => b as u16,
    }
}

fn ansi_digit(b: u8) -> u16 {
    match b {
        b'1' => 18,
        b'2' => 19,
        b'3' => 20,
        b'4' => 21,
        b'6' => 22,
        b'5' => 23,
        b'9' => 25,
        b'7' => 26,
        b'8' => 28,
        b'0' => 29,
        _ => b as u16,
    }
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

unsafe fn cg_mouse_rel(dx: i16, dy: i16, drag_button: i32) -> Result<(), InputError> {
    extern "C" {
        fn lansec_cg_mouse_rel(dx: i16, dy: i16, drag_button: i32) -> i32;
    }
    if lansec_cg_mouse_rel(dx, dy, drag_button) == 0 {
        Err(InputError::Message("CGEvent mouse rel failed".into()))
    } else {
        Ok(())
    }
}

unsafe fn cg_button(button: u8, down: bool) -> Result<(), InputError> {
    extern "C" {
        fn lansec_cg_button(button: u8, down: i32) -> i32;
    }
    if lansec_cg_button(button, i32::from(down)) == 0 {
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

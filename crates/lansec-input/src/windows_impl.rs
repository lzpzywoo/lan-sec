use lansec_protocol::InputEvent;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_HWHEEL, MOUSEINPUT, VIRTUAL_KEY,
};

use crate::InputError;

pub fn inject(ev: &InputEvent) -> Result<(), InputError> {
    unsafe {
        match *ev {
            InputEvent::MouseMoveAbs { x, y, host_w, host_h } => {
                let nx = if host_w == 0 { 0 } else { x as i32 * 65535 / host_w as i32 };
                let ny = if host_h == 0 { 0 } else { y as i32 * 65535 / host_h as i32 };
                mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, nx, ny, 0)?;
            }
            InputEvent::MouseMoveRel { dx, dy } => mouse(MOUSEEVENTF_MOVE, dx as i32, dy as i32, 0)?,
            InputEvent::MouseButton { button, down } => {
                let f = match (button, down) {
                    (0, true) => MOUSEEVENTF_LEFTDOWN,
                    (0, false) => MOUSEEVENTF_LEFTUP,
                    (1, true) => MOUSEEVENTF_RIGHTDOWN,
                    (1, false) => MOUSEEVENTF_RIGHTUP,
                    (_, true) => MOUSEEVENTF_MIDDLEDOWN,
                    (_, false) => MOUSEEVENTF_MIDDLEUP,
                };
                mouse(f, 0, 0, 0)?;
            }
            InputEvent::MouseWheel { dx, dy } => {
                if dy != 0 {
                    mouse(MOUSEEVENTF_WHEEL, 0, 0, dy as i32 * 120)?;
                }
                if dx != 0 {
                    mouse(MOUSEEVENTF_HWHEEL, 0, 0, dx as i32 * 120)?;
                }
            }
            InputEvent::Key { vk, down, .. } => key(vk, down)?,
        }
    }
    Ok(())
}

unsafe fn mouse(
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    dx: i32,
    dy: i32,
    data: i32,
) -> Result<(), InputError> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let n = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    if n == 0 {
        Err(InputError::Message("SendInput mouse failed".into()))
    } else {
        Ok(())
    }
}

unsafe fn key(vk: u16, down: bool) -> Result<(), InputError> {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if down {
                    Default::default()
                } else {
                    KEYEVENTF_KEYUP
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let n = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    if n == 0 {
        Err(InputError::Message("SendInput key failed".into()))
    } else {
        Ok(())
    }
}

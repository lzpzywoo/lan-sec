use lansec_protocol::InputEvent;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("{0}")]
    Message(String),
}

#[cfg(target_os = "macos")]
pub use macos::MouseButtons;

#[cfg(target_os = "macos")]
pub fn inject_with_buttons(ev: &InputEvent, buttons: &mut MouseButtons) -> Result<(), InputError> {
    macos::inject_with_buttons(ev, buttons)
}

pub fn inject(ev: &InputEvent) -> Result<(), InputError> {
    #[cfg(windows)]
    {
        return windows_impl::inject(ev);
    }
    #[cfg(target_os = "macos")]
    {
        return macos::inject(ev);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = ev;
        Err(InputError::Message("unsupported".into()))
    }
}

#[cfg(windows)]
mod windows_impl;
#[cfg(target_os = "macos")]
mod macos;

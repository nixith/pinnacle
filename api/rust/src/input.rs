use xkbcommon::xkb::Keysym;

use crate::{
    msg::{Args, CallbackId, KeyIntOrString, Modifier, MouseEdge, Msg},
    send_msg, CALLBACK_VEC,
};

pub struct Input;

impl Input {
    /// Set a keybind.
    ///
    /// This function takes in three parameters:
    /// - `modifiers`: A slice of the modifiers you want held for the keybind to trigger.
    /// - `key`: The key that needs to be pressed. This takes `impl Into<KeyIntOrString>` and can
    ///   take the following three types:
    ///     - [`char`]: A character of the key you want. This can be `a`, `~`, `@`, and so on.
    ///     - [`u32`]: The key in numeric form. You can use the keys defined in
    ///     [`xkbcommon::xkb::keysyms`] for this.
    ///     - [`Keysym`]: The key in `Keysym` form, from [xkbcommon::xkb::Keysym].
    pub fn keybind<F>(&self, modifiers: &[Modifier], key: impl Into<KeyIntOrString>, mut action: F)
    where
        F: FnMut() + Send + 'static,
    {
        let args_callback = move |_: Option<Args>| {
            action();
        };

        let mut callback_vec = CALLBACK_VEC.lock().unwrap();
        let len = callback_vec.len();
        callback_vec.push(Box::new(args_callback));

        let key = key.into();

        let msg = Msg::SetKeybind {
            key,
            modifiers: modifiers.to_vec(),
            callback_id: CallbackId(len as u32),
        };

        send_msg(msg).unwrap();
    }

    pub fn mousebind<F>(
        &self,
        modifiers: &[Modifier],
        button: MouseButton,
        edge: MouseEdge,
        mut action: F,
    ) where
        F: FnMut() + Send + 'static,
    {
        let args_callback = move |_: Option<Args>| {
            action();
        };

        let mut callback_vec = CALLBACK_VEC.lock().unwrap();
        let len = callback_vec.len();
        callback_vec.push(Box::new(args_callback));

        let msg = Msg::SetMousebind {
            modifiers: modifiers.to_vec(),
            button: button as u32,
            edge,
            callback_id: CallbackId(len as u32),
        };

        send_msg(msg).unwrap();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left = 0x110,
    Right,
    Middle,
    Side,
    Extra,
    Forward,
    Back,
}

impl From<char> for KeyIntOrString {
    fn from(value: char) -> Self {
        Self::String(value.to_string())
    }
}

impl From<u32> for KeyIntOrString {
    fn from(value: u32) -> Self {
        Self::Int(value)
    }
}

impl From<Keysym> for KeyIntOrString {
    fn from(value: Keysym) -> Self {
        Self::Int(value.raw())
    }
}

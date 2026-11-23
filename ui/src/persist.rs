//! Platinum's own preset files (state.json and the user's presets). The code lives in the library
//! (`platinum::presetfiles`) since the daemon reads the same files when it puts a game's preset on
//! Gaming and the preset of before back; this module keeps the window's name for it.

pub use platinum::presetfiles::*;

//! Windows platform helpers (architecture report §3.1, §8.1, §29 step 2).

mod audio;
mod console;
mod firewall;
mod games;
mod gpus;
mod hotkey;
mod monitors;
pub mod net6;
mod posters;
mod startup;
mod tray;
mod tray_mask;

pub use audio::enumerate_audio_endpoints;
pub use console::ignore_stray_ctrl_c;
pub use firewall::ensure_firewall_rule;
pub use games::{enumerate_installed_games, launch_game, poster_path_for_game_id};
pub use gpus::{enumerate_gpus, primary_gpu_vendor};
pub use hotkey::EmergencyHotkey;
pub use monitors::enumerate_monitors;
pub use startup::{set_start_at_login, start_at_login_enabled};
pub use tray::{Tray, TrayActions, TrayHandle, TrayModel};

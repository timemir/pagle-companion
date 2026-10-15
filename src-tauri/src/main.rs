// No console window behind the tray app in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    pagle_companion_lib::run();
}

// Release builds are GUI-subsystem exes: no console window on launch.
#![cfg_attr(all(not(debug_assertions), not(test)), windows_subsystem = "windows")]

#[cfg(not(target_os = "android"))]
fn main() -> eframe::Result {
    ionhf::main()
}

// On Android the library is the app, started by NativeActivity.
#[cfg(target_os = "android")]
fn main() {}

// A release build on Windows is a windowed program, so starting it does not
// open a console beside it. Debug builds keep the console for their output.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    crusteo::run();
}

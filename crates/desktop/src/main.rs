#![cfg_attr(windows, windows_subsystem = "windows")]

mod support;
#[cfg(windows)]
mod windows;

fn main() {
    #[cfg(windows)]
    std::process::exit(windows::entry());
    #[cfg(not(windows))]
    {
        // Keep workspace checks portable; no GTK/WebKit dependency on Unix.
        eprintln!("IA2.exe is the Windows desktop application. Use the IA2 server and web IDE on this platform.");
    }
}

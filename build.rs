//! Embeds icons/icon.ico into the Windows exe (Explorer, taskbar, Alt-Tab).

#[cfg(windows)]
fn main() {
    winres::WindowsResource::new()
        .set_icon("icons/icon.ico")
        .compile()
        .expect("winres icon embed failed");
}

#[cfg(not(windows))]
fn main() {}

mod app;
mod atom;
mod audio;
mod backend;
mod builtin;
mod device;
mod engine;
mod export;
mod frames;
mod lv2;
mod lv2ui;
mod project;
mod theme;
mod timeline;
mod pianoroll;
mod snippets;
mod song;
mod util;
mod waveedit;

#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;

#[cfg(target_os = "linux")]
const PW_JACK_DIRS: &[&str] = &[
    "/usr/lib/x86_64-linux-gnu/pipewire-0.3/jack",
    "/usr/lib/aarch64-linux-gnu/pipewire-0.3/jack",
    "/usr/lib64/pipewire-0.3/jack",
    "/usr/lib/pipewire-0.3/jack",
];

const USAGE: &str = "studio [OPTIONS] [PROJECT.studio]

Options:
  --jackd           Use the system libjack (a real jackd server) instead of PipeWire's JACK
  --latency N       Request a PipeWire quantum of N frames (sets PIPEWIRE_LATENCY)
  -h, --help        Show this help";

/// Behave like `pw-jack`: re-exec ourselves with PipeWire's libjack first in
/// the library search path, so the JACK client is served by PipeWire even if
/// the distribution's default libjack is the jackd2 one.
#[cfg(target_os = "linux")]
fn use_pipewire_jack() {
    if std::env::var_os("STUDIO_PW_JACK").is_some() {
        return; // already re-executed
    }
    let Some(dir) = PW_JACK_DIRS.iter().find(|d| std::path::Path::new(d).join("libjack.so.0").exists()) else {
        return; // pipewire-jack not installed; fall back to whatever libjack is present
    };
    let ld = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    if ld.split(':').any(|p| p == *dir) {
        return;
    }
    let ld = if ld.is_empty() { dir.to_string() } else { format!("{dir}:{ld}") };
    let Ok(exe) = std::env::current_exe() else { return };
    let err = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("LD_LIBRARY_PATH", ld)
        .env("STUDIO_PW_JACK", "1")
        .exec();
    eprintln!("studio: could not re-exec with PipeWire JACK: {err}");
}

fn main() -> eframe::Result {
    let mut args = std::env::args().skip(1);
    let mut use_jackd = false;
    let mut project = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--jackd" => use_jackd = true,
            "--latency" => {
                if let Some(n) = args.next()
                    && std::env::var_os("PIPEWIRE_LATENCY").is_none()
                {
                    // SAFETY: single-threaded at this point.
                    unsafe { std::env::set_var("PIPEWIRE_LATENCY", format!("{n}/48000")) };
                }
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => project = Some(std::path::PathBuf::from(a)),
        }
    }
    if !use_jackd {
        #[cfg(target_os = "linux")]
        use_pipewire_jack();
    }
    lv2::setup_lv2_path();
    // Before any other Xlib use (winit, plugin GUIs).
    lv2ui::init_x11();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Studio")
            .with_app_id("studio")
            .with_inner_size([1400.0, 860.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("Studio", options, Box::new(move |cc| Ok(Box::new(app::StudioApp::new(cc, project)))))
}

#[cfg(kani)]
mod verification;
mod vst3;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod ui_tests;

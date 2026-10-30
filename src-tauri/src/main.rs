// Prevents additional console window on Windows in all modes (debug & release)
#![windows_subsystem = "windows"]

fn main() {
    let _ = std::fs::write("d:\\Project\\lab\\termius\\rustterm_run.log", "1. main() started\n");
    std::panic::set_hook(Box::new(|info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Unknown panic payload".to_string()
        };
        let msg = format!("PANIC: {}\nLocation: {:?}\n", payload, info.location());
        let _ = std::fs::write("d:\\Project\\lab\\termius\\rustterm_panic.log", msg);
    }));
    tauri_app_lib::run();
    let _ = std::fs::write("d:\\Project\\lab\\termius\\rustterm_run.log", "2. tauri_app_lib::run() returned\n");
}

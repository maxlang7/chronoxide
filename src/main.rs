use anyhow::Result;
use calendar_tui::backend;

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--sync") | Some("--sync-full") => {
            let full = std::env::args().any(|a| a == "--sync-full");
            let cache = backend::sync(full)?;
            println!("Calendar cache: {} events", cache.events.len());
        }
        Some("--check") => {
            let cache = backend::read_cache()?;
            println!("Calendar cache: {} events", cache.events.len());
        }
        _ => println!("calendar-tui: --sync, --sync-full, --check"),
    }
    Ok(())
}

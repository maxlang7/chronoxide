use calendar_tui::backend;
mod ui;
use anyhow::Result;
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("calendar-tui · Google Calendar in the terminal\n\nUsage: calendar-tui [OPTION]\n\n  --demo       Open a read-only demo with fictional events; no credentials needed\n  --sync       Refresh the local cache if due, then exit\n  --sync-full  Force a full four-month refresh, then exit\n  --check      Report local cache counts and status without syncing\n  --version    Show the version\n  --help       Show this help\n\nCredentials: CALENDAR_TUI_CREDENTIALS or $XDG_CONFIG_HOME/calendar-tui/credentials.json");
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("calendar-tui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.len() > 2
        || args
            .get(1)
            .is_some_and(|a| !matches!(a.as_str(), "--demo" | "--sync" | "--sync-full" | "--check"))
    {
        anyhow::bail!("Use one mode at a time. Run calendar-tui --help for usage.");
    }
    if args.iter().any(|a| a == "--sync" || a == "--sync-full") {
        let cache = backend::sync(args.iter().any(|a| a == "--sync-full"))?;
        println!("Calendar cache: {} events, {} calendars; current week every 5 minutes, full range every hour",cache.events.len(),cache.calendars.len());
        return Ok(());
    }
    if args.iter().any(|a| a == "--check") {
        let cache = backend::read_cache()?;
        println!(
            "Calendar cache: {} events, coverage {:?} through {:?}, last full {}, last week {}",
            cache.events.len(),
            cache.from,
            cache.until,
            cache.last_full,
            cache.last_week
        );
        if !cache.error.is_empty() {
            anyhow::bail!("{}", cache.error);
        }
        return Ok(());
    }
    let mut terminal = ratatui::init();
    let result = ui::app(&mut terminal, args.iter().any(|a| a == "--demo"));
    ratatui::restore();
    result
}

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Duration, Local, Months, NaiveDate, TimeZone, Timelike, Utc};
use fs2::FileExt;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    time::Duration as Timeout,
};
pub fn s(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_owned()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Calendar {
    pub id: String,
    pub name: String,
    pub writable: bool,
    #[serde(default)]
    pub color: Option<[u8; 3]>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub calendar: Calendar,
    pub raw: Value,
}
impl Entry {
    pub fn title(&self) -> String {
        self.raw["summary"]
            .as_str()
            .unwrap_or("Untitled event")
            .into()
    }
    pub fn all_day(&self) -> bool {
        self.raw["start"]["date"].is_string()
    }
    pub fn start(&self) -> Result<DateTime<Local>> {
        boundary(&self.raw["start"])
    }
    pub fn end(&self) -> Result<DateTime<Local>> {
        boundary(&self.raw["end"])
    }
    pub fn overlaps(&self, from: NaiveDate, until: NaiveDate) -> bool {
        matches!((self.start(),self.end(),midnight(from),midnight(until)),(Ok(a),Ok(b),Ok(c),Ok(d)) if a<d && b>c)
    }
    pub fn minute(&self, day: NaiveDate) -> (i64, i64) {
        let a = self.start().unwrap();
        let b = self.end().unwrap();
        let start = if a.date_naive() < day {
            0
        } else {
            i64::from(a.hour() * 60 + a.minute())
        };
        let end = if b.date_naive() > day {
            1440
        } else {
            i64::from(b.hour() * 60 + b.minute())
        };
        (start, end.max(start + 1))
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cache {
    pub events: Vec<Entry>,
    pub calendars: Vec<Calendar>,
    pub from: Option<NaiveDate>,
    pub until: Option<NaiveDate>,
    pub last_full: i64,
    pub last_week: i64,
    pub error: String,
    #[serde(default)]
    pub account: String,
}
pub fn week(date: NaiveDate) -> NaiveDate {
    date - Duration::days(i64::from(date.weekday().num_days_from_monday()))
}
pub fn coverage(today: NaiveDate) -> (NaiveDate, NaiveDate) {
    (today - Months::new(2), today + Months::new(2))
}
pub fn midnight(date: NaiveDate) -> Result<DateTime<Local>> {
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .context("Invalid local midnight")
}
pub fn boundary(v: &Value) -> Result<DateTime<Local>> {
    if let Some(time) = v["dateTime"].as_str() {
        Ok(DateTime::parse_from_rfc3339(time)?.with_timezone(&Local))
    } else {
        midnight(NaiveDate::parse_from_str(
            v["date"].as_str().context("Missing event date")?,
            "%Y-%m-%d",
        )?)
    }
}
pub fn cache_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or(PathBuf::from(std::env::var("HOME")?).join(".cache"));
    Ok(base.join("calendar-tui"))
}
pub fn read_cache() -> Result<Cache> {
    match fs::read(cache_dir()?.join("events.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("Invalid calendar cache")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Cache::default()),
        Err(e) => Err(e.into()),
    }
}
fn lock() -> Result<File> {
    let dir = cache_dir()?;
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(dir.join("sync.lock"))?;
    file.lock_exclusive()?;
    Ok(file)
}
fn write_cache(cache: &Cache) -> Result<()> {
    use std::io::Write;
    let dir = cache_dir()?;
    let tmp = dir.join(format!("events.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec(cache)?)?;
    file.sync_all()?;
    fs::rename(tmp, dir.join("events.json"))?;
    Ok(())
}
fn client() -> Result<Client> {
    Ok(Client::builder().timeout(Timeout::from_secs(25)).build()?)
}
pub fn credentials_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CALENDAR_TUI_CREDENTIALS")
        .or_else(|| std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS"))
    {
        return Ok(PathBuf::from(path));
    }
    Ok(preferences_dir()?.join("credentials.json"))
}
pub fn account_label(cache: &Cache) -> &str {
    if cache.account.is_empty() {
        "Google Calendar"
    } else {
        &cache.account
    }
}
fn parse_color(value: &str) -> Option<[u8; 3]> {
    let value = value.strip_prefix('#')?;
    if value.len() != 6 || !value.is_ascii() {
        return None;
    }
    Some([
        u8::from_str_radix(&value[0..2], 16).ok()?,
        u8::from_str_radix(&value[2..4], 16).ok()?,
        u8::from_str_radix(&value[4..6], 16).ok()?,
    ])
}
fn token(client: &Client) -> Result<String> {
    let path = credentials_path()?;
    let data:Value=serde_json::from_slice(&fs::read(&path).with_context(||format!("Cannot read OAuth credentials at {}. Set CALENDAR_TUI_CREDENTIALS to an authorized-user JSON file.",path.display()))?)?;
    let expiry = s(&data, "expiry");
    let parsed = DateTime::parse_from_rfc3339(&expiry)
        .or_else(|_| DateTime::parse_from_rfc3339(&format!("{expiry}Z")));
    if parsed.is_ok_and(|e| e > Utc::now() + Duration::minutes(2)) && !s(&data, "token").is_empty()
    {
        return Ok(s(&data, "token"));
    }
    for key in ["client_id", "client_secret", "refresh_token"] {
        if s(&data, key).is_empty() {
            bail!("OAuth credentials are missing {key}. Supply an authorized-user JSON file, not an OAuth client download or service account key.");
        }
    }
    let response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", s(&data, "client_id")),
            ("client_secret", s(&data, "client_secret")),
            ("refresh_token", s(&data, "refresh_token")),
            ("grant_type", "refresh_token".into()),
        ])
        .send()?;
    if !response.status().is_success() {
        bail!("Google authorization expired. Reauthorize your OAuth credentials file.");
    }
    let data: Value = response.json()?;
    let token = s(&data, "access_token");
    if token.is_empty() {
        bail!("Google returned no access token");
    }
    Ok(token)
}
fn read_page(client: &Client, token: &str, url: &str, params: &[(&str, String)]) -> Result<Value> {
    let mut failure = None;
    for attempt in 0..3 {
        match client.get(url).bearer_auth(token).query(params).send() {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    match response.json() {
                        Ok(data) => return Ok(data),
                        Err(e) => failure = Some(anyhow::Error::from(e)),
                    }
                } else if status.is_server_error() || status.as_u16() == 429 {
                    failure = Some(anyhow::anyhow!("Google Calendar returned {status}"));
                } else {
                    bail!("Google Calendar returned {status}");
                }
            }
            Err(e) => failure = Some(e.into()),
        }
        if attempt < 2 {
            std::thread::sleep(Timeout::from_millis(500 * (attempt + 1)));
        }
    }
    Err(failure.unwrap())
}
fn pages(
    client: &Client,
    token: &str,
    url: &str,
    mut params: Vec<(&str, String)>,
) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    loop {
        let data = read_page(client, token, url, &params)?;
        if let Some(rows) = data["items"].as_array() {
            items.extend(rows.iter().cloned());
        }
        let next = s(&data, "nextPageToken");
        if next.is_empty() {
            break;
        }
        params.retain(|(key, _)| *key != "pageToken");
        params.push(("pageToken", next));
    }
    Ok(items)
}
fn url(calendar: &str, event: Option<&str>) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse("https://www.googleapis.com/calendar/v3/calendars/")?;
    {
        let mut path = url.path_segments_mut().unwrap();
        path.pop_if_empty().push(calendar).push("events");
        if let Some(event) = event {
            path.push(event);
        }
    }
    Ok(url)
}
pub fn full_due(cache: &Cache, today: NaiveDate, now: i64) -> bool {
    let (from, until) = coverage(today);
    now - cache.last_full >= 3600 || cache.from != Some(from) || cache.until != Some(until)
}
fn merge(
    cache: &mut Cache,
    calendar: &Calendar,
    rows: Vec<Value>,
    from: NaiveDate,
    until: NaiveDate,
) {
    // Replacing a successful range also removes deleted events and moved instances.
    cache
        .events
        .retain(|e| e.calendar.id != calendar.id || !e.overlaps(from, until));
    for raw in rows {
        if raw["status"] == "cancelled" {
            continue;
        }
        let entry = Entry {
            calendar: calendar.clone(),
            raw,
        };
        if entry.start().is_err() || entry.end().is_err() {
            continue;
        }
        // A multi-day event can also be present outside the replaced range.
        cache
            .events
            .retain(|e| e.calendar.id != entry.calendar.id || e.raw["id"] != entry.raw["id"]);
        cache.events.push(entry);
    }
}
fn fetch(cache: &mut Cache, full: bool) -> Result<()> {
    let today = Local::now().date_naive();
    let now = Utc::now().timestamp();
    let (from, until) = if full {
        coverage(today)
    } else {
        let from = week(today);
        (from, from + Duration::days(7))
    };
    let client = client()?;
    let token = token(&client)?;
    let calendars = pages(
        &client,
        &token,
        "https://www.googleapis.com/calendar/v3/users/me/calendarList",
        vec![],
    )?;
    cache.account = calendars
        .iter()
        .find(|c| c["primary"] == true)
        .map(|c| s(c, "id"))
        .unwrap_or_default();
    let calendars: Vec<Calendar> = calendars
        .into_iter()
        .filter(|c| c["hidden"] != true && c["selected"] != false)
        .map(|c| Calendar {
            id: s(&c, "id"),
            name: s(&c, "summary"),
            writable: matches!(c["accessRole"].as_str(), Some("owner" | "writer")),
            color: parse_color(&s(&c, "backgroundColor")),
        })
        .collect();
    let mut errors = Vec::new();
    for calendar in &calendars {
        match pages(
            &client,
            &token,
            url(&calendar.id, None)?.as_str(),
            vec![
                ("timeMin", midnight(from)?.to_rfc3339()),
                ("timeMax", midnight(until)?.to_rfc3339()),
                ("singleEvents", "true".into()),
                ("orderBy", "startTime".into()),
                ("maxResults", "2500".into()),
            ],
        ) {
            Ok(rows) => merge(cache, calendar, rows, from, until),
            Err(e) => errors.push(format!("{}: {e:#}", calendar.name)),
        }
    }
    cache
        .events
        .retain(|e| calendars.iter().any(|c| c.id == e.calendar.id));
    for entry in &mut cache.events {
        if let Some(c) = calendars.iter().find(|c| c.id == entry.calendar.id) {
            entry.calendar = c.clone();
        }
    }
    cache.calendars = calendars;
    if full {
        let (a, b) = coverage(today);
        cache.events.retain(|e| e.overlaps(a, b));
        if errors.is_empty() {
            cache.from = Some(a);
            cache.until = Some(b);
            cache.last_full = now;
        }
    }
    if errors.is_empty() {
        cache.last_week = now;
    }
    cache.error = errors.join(" | ");
    Ok(())
}
pub fn sync(force: bool) -> Result<Cache> {
    let _lock = lock()?;
    let mut cache = read_cache()?;
    let now = Utc::now().timestamp();
    let full = force || full_due(&cache, Local::now().date_naive(), now);
    if !full && now - cache.last_week < 300 {
        return Ok(cache);
    }
    if let Err(e) = fetch(&mut cache, full) {
        cache.error = format!("{e:#}");
    }
    write_cache(&cache)?;
    if !cache.error.is_empty() {
        bail!("{}", cache.error);
    }
    Ok(cache)
}
pub const LABELS: [&str; 5] = ["Title", "Start", "End", "Location", "Description"];
pub fn fields(entry: &Entry) -> [String; 5] {
    let format = |key| {
        if entry.all_day() {
            s(&entry.raw[key], "date")
        } else {
            boundary(&entry.raw[key])
                .map(|t| t.format("%Y-%m-%d %-I:%M %p").to_string())
                .unwrap_or_default()
        }
    };
    [
        entry.title(),
        format("start"),
        format("end"),
        s(&entry.raw, "location"),
        s(&entry.raw, "description"),
    ]
}
fn parse_edit_time(value: &str) -> Result<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%d %I:%M %p")
        .context("Use YYYY-MM-DD h:mm AM/PM, for example 2026-10-28 3:30 PM")
}
pub fn patch(entry: &Entry, fields: &[String; 5]) -> Result<Value> {
    if !entry.calendar.writable {
        bail!("This calendar is read-only");
    }
    if fields[0].trim().is_empty() {
        bail!("Title cannot be empty");
    }
    let original = self::fields(entry);
    let mut patch = json!({});
    for (i, key) in [(0, "summary"), (3, "location"), (4, "description")] {
        if fields[i] != original[i] {
            patch[key] = json!(fields[i]);
        }
    }
    if fields[1] != original[1] || fields[2] != original[2] {
        if entry.all_day() {
            let a = NaiveDate::parse_from_str(&fields[1], "%Y-%m-%d")
                .context("Start must be YYYY-MM-DD")?;
            let b = NaiveDate::parse_from_str(&fields[2], "%Y-%m-%d")
                .context("End must be YYYY-MM-DD, exclusive")?;
            if b <= a {
                bail!("End must be after start; all-day end dates are exclusive");
            }
            patch["start"] = json!({"date":a.to_string()});
            patch["end"] = json!({"date":b.to_string()});
        } else {
            let parse = |value: &str| -> Result<DateTime<Local>> {
                let date = parse_edit_time(value)?;
                Local
                    .from_local_datetime(&date)
                    .single()
                    .context("Time is ambiguous or nonexistent due to DST; choose a different time")
            };
            let a = parse(&fields[1])?;
            let b = parse(&fields[2])?;
            if b <= a {
                bail!("End must be after start");
            }
            patch["start"] = json!({"dateTime":a.to_rfc3339()});
            patch["end"] = json!({"dateTime":b.to_rfc3339()});
        }
    }
    Ok(patch)
}
pub fn save(entry: Entry, patch: Value) -> Result<Cache> {
    if !entry.calendar.writable {
        bail!("Read-only calendar");
    }
    let etag = s(&entry.raw, "etag");
    if etag.is_empty() {
        bail!("Missing event version. Refresh before editing.");
    }
    let _lock = lock()?;
    let client = client()?;
    let token = token(&client)?;
    let url = url(&entry.calendar.id, Some(&s(&entry.raw, "id")))?;
    let response = client
        .patch(url.clone())
        .bearer_auth(&token)
        .header("If-Match", etag)
        .query(&[("sendUpdates", "none")])
        .json(&patch)
        .send()?;
    if response.status() == reqwest::StatusCode::PRECONDITION_FAILED {
        bail!("This event changed on Google. Close this popup and refresh before editing again.");
    }
    if !response.status().is_success() {
        bail!("Save failed: Google returned {}", response.status());
    }
    // Verify the stored result through a fresh GET before reporting success.
    let response = client.get(url).bearer_auth(token).send()?;
    if !response.status().is_success() {
        bail!("Save was sent but verification failed. Refresh before retrying.");
    }
    let raw: Value = response.json()?;
    for key in ["summary", "location", "description", "start", "end"] {
        if let Some(expected) = patch.get(key) {
            let matches = if matches!(key, "start" | "end") {
                boundary(expected)? == boundary(&raw[key])?
            } else {
                expected == &raw[key]
            };
            if !matches {
                bail!("Google returned a different {key}. Refresh to review the saved event.");
            }
        }
    }
    let mut cache = read_cache()?;
    cache
        .events
        .retain(|e| e.calendar.id != entry.calendar.id || e.raw["id"] != entry.raw["id"]);
    cache.events.push(Entry {
        calendar: entry.calendar,
        raw,
    });
    write_cache(&cache)?;
    Ok(cache)
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Preferences {
    pub hidden_calendars: std::collections::BTreeSet<String>,
}
impl Preferences {
    pub fn enabled(&self, id: &str) -> bool {
        !self.hidden_calendars.contains(id)
    }
    pub fn toggle(&mut self, id: &str) {
        if !self.hidden_calendars.remove(id) {
            self.hidden_calendars.insert(id.into());
        }
    }
}
fn preferences_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(PathBuf::from(std::env::var("HOME")?).join(".config"));
    Ok(base.join("calendar-tui"))
}
pub fn read_preferences() -> Result<Preferences> {
    match fs::read(preferences_dir()?.join("preferences.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("Invalid calendar preferences")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Preferences::default()),
        Err(e) => Err(e.into()),
    }
}
pub fn write_preferences(preferences: &Preferences) -> Result<()> {
    use std::io::Write;
    let dir = preferences_dir()?;
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let tmp = dir.join(format!("preferences.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(preferences)?)?;
    file.sync_all()?;
    fs::rename(tmp, dir.join("preferences.json"))?;
    Ok(())
}
pub fn demo_cache() -> Cache {
    let today = Local::now().date_naive();
    let start = week(today);
    let calendars: Vec<Calendar> = [
        ("classes", "Classes", [66, 154, 142]),
        ("work", "Work", [75, 153, 210]),
        ("personal", "Personal", [164, 121, 177]),
        ("projects", "Projects", [214, 93, 14]),
    ]
    .into_iter()
    .map(|(id, name, color)| Calendar {
        id: id.into(),
        name: name.into(),
        writable: false,
        color: Some(color),
    })
    .collect();
    let mut events = Vec::new();
    for (day, hour, minute, duration, title, cal) in [
        (0, 9, 0, 60, "Physics", 0),
        (0, 11, 0, 90, "Team planning", 1),
        (0, 15, 0, 90, "Project work", 3),
        (1, 9, 30, 60, "Design review", 1),
        (1, 11, 0, 60, "Chemistry", 0),
        (1, 14, 0, 120, "Study group", 0),
        (2, 9, 0, 60, "Physics", 0),
        (2, 11, 0, 90, "Office hours", 1),
        (2, 15, 0, 90, "Project work", 3),
        (3, 9, 30, 60, "Writing", 0),
        (3, 13, 0, 90, "Lab meeting", 3),
        (3, 17, 0, 60, "Dinner", 2),
        (4, 9, 0, 60, "Physics", 0),
        (4, 11, 0, 60, "Weekly review", 1),
        (4, 14, 0, 120, "Project work", 3),
        (5, 10, 0, 90, "Coffee", 2),
        (5, 14, 0, 120, "Hike", 2),
        (6, 11, 0, 60, "Reading", 2),
    ] {
        let date = start + Duration::days(day);
        let begin = Local
            .from_local_datetime(&date.and_hms_opt(hour, minute, 0).unwrap())
            .single()
            .unwrap();
        events.push(Entry{calendar:calendars[cal].clone(),raw:json!({"id":format!("demo-{}",events.len()),"summary":title,"start":{"dateTime":begin.to_rfc3339()},"end":{"dateTime":(begin+Duration::minutes(duration)).to_rfc3339()}})});
    }
    Cache {
        events,
        calendars,
        account: "Demo · fictional events".into(),
        ..Cache::default()
    }
}
#[cfg(test)]
mod preference_tests {
    use super::*;
    #[test]
    fn toggles_persist_by_id() {
        let mut p = Preferences::default();
        assert!(p.enabled("first"));
        p.toggle("first");
        assert!(!p.enabled("first"));
        assert!(p.enabled("second"));
        let json = serde_json::to_vec(&p).unwrap();
        let mut restored: Preferences = serde_json::from_slice(&json).unwrap();
        assert!(!restored.enabled("first"));
        restored.toggle("first");
        assert!(restored.enabled("first"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry() -> Entry {
        Entry {
            calendar: Calendar {
                id: "test".into(),
                name: "Test".into(),
                writable: true,
                color: None,
            },
            raw: json!({"id":"x","summary":"Original","start":{"date":"2026-11-01"},"end":{"date":"2026-11-02"}}),
        }
    }
    #[test]
    fn us_editor_times_distinguish_noon_and_midnight() {
        assert_eq!(parse_edit_time("2026-10-28 12:00 AM").unwrap().hour(), 0);
        assert_eq!(parse_edit_time("2026-10-28 12:00 PM").unwrap().hour(), 12);
        assert_eq!(parse_edit_time("2026-10-28 3:30 PM").unwrap().hour(), 15);
        assert!(parse_edit_time("2026-10-28 15:30").is_err());
    }
    #[test]
    fn all_day_date_is_preserved() {
        assert_eq!(
            entry().start().unwrap().date_naive().to_string(),
            "2026-11-01"
        );
    }
    #[test]
    fn cache_range_is_four_months() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 28).unwrap();
        assert_eq!(
            coverage(d),
            (
                NaiveDate::from_ymd_opt(2026, 8, 28).unwrap(),
                NaiveDate::from_ymd_opt(2026, 12, 28).unwrap()
            )
        );
    }
    #[test]
    fn cadence() {
        let d = Local::now().date_naive();
        let (a, b) = coverage(d);
        let c = Cache {
            from: Some(a),
            until: Some(b),
            last_full: 10000,
            ..Cache::default()
        };
        assert!(!full_due(&c, d, 13599));
        assert!(full_due(&c, d, 13600));
        assert!(full_due(&c, d + Duration::days(1), 10001));
    }
    #[test]
    fn range_replacement_removes_deleted_events() {
        let e = entry();
        let mut c = Cache {
            events: vec![e.clone()],
            ..Cache::default()
        };
        let d = e.start().unwrap().date_naive();
        merge(&mut c, &e.calendar, vec![], d, d + Duration::days(7));
        assert!(c.events.is_empty());
    }
    #[test]
    fn failed_calendar_is_not_replaced() {
        let e = entry();
        let mut other = e.clone();
        other.calendar.id = "other".into();
        let mut c = Cache {
            events: vec![e.clone(), other],
            ..Cache::default()
        };
        let d = e.start().unwrap().date_naive();
        merge(&mut c, &e.calendar, vec![], d, d + Duration::days(7));
        assert_eq!(c.events.len(), 1);
        assert_eq!(c.events[0].calendar.id, "other");
    }
    #[test]
    fn patch_changes_only_edited_fields() {
        let e = entry();
        let mut f = fields(&e);
        f[0] = "New".into();
        assert_eq!(patch(&e, &f).unwrap(), json!({"summary":"New"}));
    }
    #[test]
    fn invalid_end_is_rejected() {
        let e = entry();
        let mut f = fields(&e);
        f[2] = "2026-10-30".into();
        assert!(patch(&e, &f).is_err());
    }
    #[test]
    fn readonly_edit_is_rejected() {
        let mut e = entry();
        e.calendar.writable = false;
        assert!(patch(&e, &fields(&e)).is_err());
    }
    #[test]
    fn multi_day_overlaps_both_days() {
        let mut e = entry();
        e.raw["end"] = json!({"date":"2026-11-04"});
        let d = NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        assert!(e.overlaps(d, d + Duration::days(1)));
        assert!(!e.overlaps(d + Duration::days(1), d + Duration::days(2)));
    }
}

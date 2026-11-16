//! calendar_* and reminders_* tools.

use super::CalendarBackend;
use super::ics::{Event, Reminder, When, format_local, parse_local};
#[cfg(test)]
use async_trait::async_trait;
use crate::tools::{Registry, ToolOutput, typed_tool};
use chrono::{DateTime, Days, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;

#[derive(Deserialize, schemars::JsonSchema)]
struct ListArgs {
    /// Local date or time (YYYY-MM-DD or YYYY-MM-DDTHH:MM). Default: today.
    from: Option<String>,
    /// Inclusive local date, or time. Default: from + 7 days.
    to: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CreateArgs {
    title: String,
    /// YYYY-MM-DDTHH:MM (local) for a timed event, YYYY-MM-DD for a whole day.
    start: String,
    /// Same form as start; a date is the last day included. Default: start + 1 h, or the same day.
    end: Option<String>,
    location: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct UpdateArgs {
    uid: String,
    title: Option<String>,
    /// Moving start alone keeps the duration.
    start: Option<String>,
    end: Option<String>,
    location: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct UidArgs {
    uid: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RemindArgs {
    text: String,
    /// YYYY-MM-DDTHH:MM (local); a date alone means 09:00.
    at: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RemindersListArgs {
    include_done: Option<bool>,
}

fn midnight(d: NaiveDate, tz: Tz) -> DateTime<Utc> {
    tz.from_local_datetime(&d.and_hms_opt(0, 0, 0).expect("valid time"))
        .earliest()
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|| d.and_hms_opt(0, 0, 0).expect("valid time").and_utc())
}

fn sort_key(w: &When, tz: Tz) -> DateTime<Utc> {
    match w {
        When::Date(d) => midnight(*d, tz),
        When::At(t) => *t,
    }
}

fn default_end(start: &When) -> When {
    match start {
        When::Date(d) => When::Date(*d + Days::new(1)),
        When::At(t) => When::At(*t + Duration::hours(1)),
    }
}

/// End given by the agent (a date is inclusive) → stored end (a date is exclusive).
fn parse_end(start: &When, end: &str, tz: Tz) -> Result<When, String> {
    let end = match (start, parse_local(end, tz)?) {
        (When::Date(_), When::Date(d)) => When::Date(d + Days::new(1)),
        (When::At(_), When::At(t)) => When::At(t),
        _ => return Err("start and end must both be dates or both be times".into()),
    };
    check_order(start, &end)?;
    Ok(end)
}

fn check_order(start: &When, end: &When) -> Result<(), String> {
    let ok = match (start, end) {
        (When::Date(s), When::Date(e)) => e > s,
        (When::At(s), When::At(e)) => e > s,
        _ => false,
    };
    if ok { Ok(()) } else { Err("end must be after start".into()) }
}

fn event_json(e: &Event, tz: Tz) -> Value {
    let mut m = Map::new();
    m.insert("uid".into(), json!(e.uid));
    m.insert("title".into(), json!(e.title));
    m.insert("start".into(), json!(format_local(&e.start, tz)));
    let end = e.end.map(|end| match end {
        When::Date(d) => When::Date(d.pred_opt().unwrap_or(d)),
        at => at,
    });
    if let Some(end) = end {
        m.insert("end".into(), json!(format_local(&end, tz)));
    }
    if let Some(l) = &e.location {
        m.insert("location".into(), json!(l));
    }
    if let Some(d) = &e.description {
        m.insert("description".into(), json!(d));
    }
    Value::Object(m)
}

fn reminder_json(r: &Reminder, tz: Tz) -> Value {
    json!({"uid": r.uid, "text": r.text, "at": format_local(&When::At(r.due), tz), "done": r.done})
}

fn reminder_time(at: &str, tz: Tz) -> Result<DateTime<Utc>, String> {
    Ok(match parse_local(at, tz)? {
        When::At(t) => t,
        When::Date(d) => midnight(d, tz) + Duration::hours(9),
    })
}

fn not_found(kind: &str, uid: &str, list: &str) -> ToolOutput {
    ToolOutput::error(format!("no {kind} with uid {uid:?}; call {list} to get current uids"))
}

pub fn register_calendar_tools(reg: &Registry, backend: Arc<dyn CalendarBackend>, tz: Tz) -> anyhow::Result<()> {
    let b = backend.clone();
    let (d, h) = typed_tool(
        "calendar_list",
        "List events between two dates (local time). Defaults to the next 7 days.",
        false,
        move |a: ListArgs| {
            let b = b.clone();
            async move {
                let from = match a.from.as_deref().map(|s| parse_local(s, tz)) {
                    None => midnight(Utc::now().with_timezone(&tz).date_naive(), tz),
                    Some(Ok(w)) => sort_key(&w, tz),
                    Some(Err(e)) => return ToolOutput::error(e),
                };
                let to = match a.to.as_deref().map(|s| parse_local(s, tz)) {
                    None => from + Duration::days(7),
                    Some(Ok(When::Date(d))) => midnight(d + Days::new(1), tz),
                    Some(Ok(When::At(t))) => t,
                    Some(Err(e)) => return ToolOutput::error(e),
                };
                match b.list_events(from, to).await {
                    Ok(mut events) => {
                        events.sort_by_key(|e| sort_key(&e.start, tz));
                        ToolOutput::ok(events.iter().map(|e| event_json(e, tz)).collect::<Vec<_>>())
                    }
                    Err(e) => ToolOutput::error(format!("{e:#}")),
                }
            }
        },
    );
    reg.register(d, h)?;

    let b = backend.clone();
    let (d, h) = typed_tool(
        "calendar_create",
        "Add an event: something with a time slot. For 'remind me', use reminders_create.",
        false,
        move |a: CreateArgs| {
            let b = b.clone();
            async move {
                let start = match parse_local(&a.start, tz) {
                    Ok(s) => s,
                    Err(e) => return ToolOutput::error(e),
                };
                let end = match a.end.as_deref().map(|e| parse_end(&start, e, tz)) {
                    None => default_end(&start),
                    Some(Ok(e)) => e,
                    Some(Err(e)) => return ToolOutput::error(e),
                };
                let event = Event {
                    uid: uuid::Uuid::new_v4().to_string(),
                    title: a.title,
                    start,
                    end: Some(end),
                    location: a.location,
                    description: a.description,
                };
                match b.put_event(&event).await {
                    Ok(()) => ToolOutput::ok(event_json(&event, tz)),
                    Err(e) => ToolOutput::error(format!("{e:#}")),
                }
            }
        },
    );
    reg.register(d, h)?;

    let b = backend.clone();
    let (d, h) = typed_tool("calendar_update", "Change an event. Get its uid from calendar_list.", false, move |a: UpdateArgs| {
        let b = b.clone();
        async move {
            let mut e = match b.get_event(&a.uid).await {
                Ok(Some(e)) => e,
                Ok(None) => return not_found("event", &a.uid, "calendar_list"),
                Err(e) => return ToolOutput::error(format!("{e:#}")),
            };
            let old_start = e.start;
            if let Some(s) = &a.start {
                match parse_local(s, tz) {
                    Ok(s) => e.start = s,
                    Err(err) => return ToolOutput::error(err),
                }
            }
            e.end = match (&a.end, &a.start, e.end) {
                (Some(end), _, _) => match parse_end(&e.start, end, tz) {
                    Ok(end) => Some(end),
                    Err(err) => return ToolOutput::error(err),
                },
                (None, Some(_), Some(old_end)) => Some(match (old_start, e.start, old_end) {
                    (When::At(os), When::At(ns), When::At(oe)) => When::At(oe + (ns - os)),
                    (When::Date(os), When::Date(ns), When::Date(oe)) => When::Date(oe + (ns - os)),
                    _ => default_end(&e.start),
                }),
                (None, _, end) => end,
            };
            if let Some(end) = &e.end
                && let Err(err) = check_order(&e.start, end)
            {
                return ToolOutput::error(err);
            }
            if let Some(t) = a.title {
                e.title = t;
            }
            if a.location.is_some() {
                e.location = a.location;
            }
            if a.description.is_some() {
                e.description = a.description;
            }
            match b.put_event(&e).await {
                Ok(()) => ToolOutput::ok(event_json(&e, tz)),
                Err(err) => ToolOutput::error(format!("{err:#}")),
            }
        }
    });
    reg.register(d, h)?;

    let b = backend.clone();
    let (d, h) = typed_tool(
        "calendar_delete",
        "Delete an event. Confirm with the user first unless they asked for it explicitly.",
        false,
        move |a: UidArgs| {
            let b = b.clone();
            async move {
                match b.delete_event(&a.uid).await {
                    Ok(true) => ToolOutput::ok(json!({"deleted": a.uid})),
                    Ok(false) => not_found("event", &a.uid, "calendar_list"),
                    Err(e) => ToolOutput::error(format!("{e:#}")),
                }
            }
        },
    );
    reg.register(d, h)?;

    let b = backend.clone();
    let (d, h) = typed_tool("reminders_create", "Remind the user of something at a given time.", false, move |a: RemindArgs| {
        let b = b.clone();
        async move {
            let due = match reminder_time(&a.at, tz) {
                Ok(t) => t,
                Err(e) => return ToolOutput::error(e),
            };
            let r = Reminder { uid: uuid::Uuid::new_v4().to_string(), text: a.text, due, done: false };
            match b.put_reminder(&r).await {
                Ok(()) => ToolOutput::ok(reminder_json(&r, tz)),
                Err(e) => ToolOutput::error(format!("{e:#}")),
            }
        }
    });
    reg.register(d, h)?;

    let b = backend.clone();
    let (d, h) = typed_tool("reminders_list", "List pending reminders, soonest first.", false, move |a: RemindersListArgs| {
        let b = b.clone();
        async move {
            match b.list_reminders().await {
                Ok(mut rs) => {
                    if !a.include_done.unwrap_or(false) {
                        rs.retain(|r| !r.done);
                    }
                    rs.sort_by_key(|r| r.due);
                    ToolOutput::ok(rs.iter().map(|r| reminder_json(r, tz)).collect::<Vec<_>>())
                }
                Err(e) => ToolOutput::error(format!("{e:#}")),
            }
        }
    });
    reg.register(d, h)?;

    let b = backend.clone();
    let (d, h) = typed_tool("reminders_done", "Mark a reminder as done.", false, move |a: UidArgs| {
        let b = b.clone();
        async move {
            match b.get_reminder(&a.uid).await {
                Ok(Some(mut r)) => {
                    r.done = true;
                    match b.put_reminder(&r).await {
                        Ok(()) => ToolOutput::ok(reminder_json(&r, tz)),
                        Err(e) => ToolOutput::error(format!("{e:#}")),
                    }
                }
                Ok(None) => not_found("reminder", &a.uid, "reminders_list"),
                Err(e) => ToolOutput::error(format!("{e:#}")),
            }
        }
    });
    reg.register(d, h)?;

    let b = backend;
    let (d, h) = typed_tool("reminders_delete", "Delete a reminder.", false, move |a: UidArgs| {
        let b = b.clone();
        async move {
            match b.delete_reminder(&a.uid).await {
                Ok(true) => ToolOutput::ok(json!({"deleted": a.uid})),
                Ok(false) => not_found("reminder", &a.uid, "reminders_list"),
                Err(e) => ToolOutput::error(format!("{e:#}")),
            }
        }
    });
    reg.register(d, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Registry;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeBackend {
        events: Mutex<Vec<Event>>,
        reminders: Mutex<Vec<Reminder>>,
    }

    #[async_trait]
    impl CalendarBackend for FakeBackend {
        async fn list_events(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> anyhow::Result<Vec<Event>> {
            Ok(self.events.lock().unwrap().iter().filter(|e| match e.start {
                When::At(t) => t >= from && t < to,
                When::Date(_) => true,
            }).cloned().collect())
        }
        async fn get_event(&self, uid: &str) -> anyhow::Result<Option<Event>> {
            Ok(self.events.lock().unwrap().iter().find(|e| e.uid == uid).cloned())
        }
        async fn put_event(&self, e: &Event) -> anyhow::Result<()> {
            let mut v = self.events.lock().unwrap();
            v.retain(|x| x.uid != e.uid);
            v.push(e.clone());
            Ok(())
        }
        async fn delete_event(&self, uid: &str) -> anyhow::Result<bool> {
            let mut v = self.events.lock().unwrap();
            let n = v.len();
            v.retain(|x| x.uid != uid);
            Ok(v.len() != n)
        }
        async fn list_reminders(&self) -> anyhow::Result<Vec<Reminder>> {
            Ok(self.reminders.lock().unwrap().clone())
        }
        async fn get_reminder(&self, uid: &str) -> anyhow::Result<Option<Reminder>> {
            Ok(self.reminders.lock().unwrap().iter().find(|r| r.uid == uid).cloned())
        }
        async fn put_reminder(&self, r: &Reminder) -> anyhow::Result<()> {
            let mut v = self.reminders.lock().unwrap();
            v.retain(|x| x.uid != r.uid);
            v.push(r.clone());
            Ok(())
        }
        async fn delete_reminder(&self, uid: &str) -> anyhow::Result<bool> {
            let mut v = self.reminders.lock().unwrap();
            let n = v.len();
            v.retain(|x| x.uid != uid);
            Ok(v.len() != n)
        }
    }

    fn setup() -> Registry {
        let reg = Registry::new();
        register_calendar_tools(&reg, Arc::new(FakeBackend::default()), chrono_tz::Europe::Paris).unwrap();
        reg
    }

    #[tokio::test]
    async fn create_then_list_in_local_time() {
        let reg = setup();
        let out = reg.call("calendar_create", json!({"title": "Dentiste", "start": "2026-11-20T10:00", "location": "Lyon"})).await;
        assert!(!out.is_error, "{}", out.to_text());
        let uid = out.value["uid"].as_str().unwrap().to_string();
        let list = reg.call("calendar_list", json!({"from": "2026-11-20", "to": "2026-11-20"})).await;
        assert_eq!(list.value[0]["uid"], uid);
        assert_eq!(list.value[0]["start"], "2026-11-20T10:00+01:00");
        assert_eq!(list.value[0]["end"], "2026-11-20T11:00+01:00");
        assert_eq!(list.value[0]["location"], "Lyon");
        let none = reg.call("calendar_list", json!({"from": "2026-11-21", "to": "2026-11-22"})).await;
        assert_eq!(none.value, json!([]));
    }

    #[tokio::test]
    async fn end_defaults() {
        let reg = setup();
        reg.call("calendar_create", json!({"title": "Vacances", "start": "2026-12-24"})).await;
        reg.call("calendar_create", json!({"title": "Pont", "start": "2026-12-28", "end": "2026-12-30"})).await;
        let list = reg.call("calendar_list", json!({"from": "2026-12-01", "to": "2026-12-31"})).await;
        let ev: Vec<_> = list.value.as_array().unwrap().iter().map(|e| (e["title"].as_str().unwrap(), e["start"].as_str().unwrap(), e["end"].as_str().unwrap())).collect();
        assert_eq!(ev, vec![("Vacances", "2026-12-24", "2026-12-24"), ("Pont", "2026-12-28", "2026-12-30")]);
    }

    #[tokio::test]
    async fn update_keeps_unset_fields() {
        let reg = setup();
        let out = reg.call("calendar_create", json!({"title": "Dentiste", "start": "2026-11-20T10:00", "location": "Lyon"})).await;
        let uid = out.value["uid"].as_str().unwrap();
        let up = reg.call("calendar_update", json!({"uid": uid, "start": "2026-11-20T15:00"})).await;
        assert!(!up.is_error, "{}", up.to_text());
        let list = reg.call("calendar_list", json!({"from": "2026-11-20", "to": "2026-11-20"})).await;
        assert_eq!(list.value[0]["title"], "Dentiste");
        assert_eq!(list.value[0]["location"], "Lyon");
        assert_eq!(list.value[0]["start"], "2026-11-20T15:00+01:00");
        assert_eq!(list.value[0]["end"], "2026-11-20T16:00+01:00", "duration is kept when only start moves");
    }

    #[tokio::test]
    async fn unknown_uid_explains() {
        let reg = setup();
        for (tool, hint) in [("calendar_update", "calendar_list"), ("calendar_delete", "calendar_list"), ("reminders_done", "reminders_list"), ("reminders_delete", "reminders_list")] {
            let out = reg.call(tool, json!({"uid": "nope"})).await;
            assert!(out.is_error && out.to_text().contains(hint), "{tool}: {}", out.to_text());
        }
    }

    #[tokio::test]
    async fn reminders_sorted_and_done_hidden() {
        let reg = setup();
        reg.call("reminders_create", json!({"text": "B", "at": "2026-11-20T18:00"})).await;
        let a = reg.call("reminders_create", json!({"text": "A", "at": "2026-11-19T08:00"})).await;
        reg.call("reminders_create", json!({"text": "C", "at": "2026-11-21"})).await;
        let list = reg.call("reminders_list", json!({})).await;
        let texts: Vec<_> = list.value.as_array().unwrap().iter().map(|r| r["text"].as_str().unwrap().to_string()).collect();
        assert_eq!(texts, vec!["A", "B", "C"]);
        assert_eq!(list.value[2]["at"], "2026-11-21T09:00+01:00", "date alone means 09:00");
        reg.call("reminders_done", json!({"uid": a.value["uid"]})).await;
        assert_eq!(reg.call("reminders_list", json!({})).await.value.as_array().unwrap().len(), 2);
        assert_eq!(reg.call("reminders_list", json!({"include_done": true})).await.value.as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn bad_date_explains() {
        let reg = setup();
        let out = reg.call("calendar_create", json!({"title": "X", "start": "demain"})).await;
        assert!(out.is_error && out.to_text().contains("YYYY-MM-DD"));
        let out = reg.call("calendar_create", json!({"title": "X", "start": "2026-11-20T10:00", "end": "2026-11-20T09:00"})).await;
        assert!(out.is_error && out.to_text().contains("after"), "{}", out.to_text());
    }
}

//! calendar_* and reminders_* tools.

use crate::backend::CalendarBackend;
use crate::ics::{Event, Reminder, When, format_local, parse_local};
use chrono::{DateTime, Days, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ListArgs {
    /// Local date or time (YYYY-MM-DD or YYYY-MM-DDTHH:MM). Default: today.
    from: Option<String>,
    /// Inclusive local date, or time. Default: from + 7 days.
    to: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct CreateArgs {
    title: String,
    /// YYYY-MM-DDTHH:MM (local) for a timed event, YYYY-MM-DD for a whole day.
    start: String,
    /// Same form as start; a date is the last day included. Default: start + 1 h, or the same day.
    end: Option<String>,
    location: Option<String>,
    description: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct UpdateArgs {
    uid: String,
    title: Option<String>,
    /// Moving start alone keeps the duration.
    start: Option<String>,
    end: Option<String>,
    location: Option<String>,
    description: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct UidArgs {
    uid: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct RemindArgs {
    text: String,
    /// YYYY-MM-DDTHH:MM (local); a date alone means 09:00.
    at: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct RemindersListArgs {
    include_done: Option<bool>,
}

/// A tool's result: JSON, or an error message for the agent.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub value: Value,
    pub is_error: bool,
}

impl Output {
    pub fn ok(v: impl Serialize) -> Self {
        let value = serde_json::to_value(v).unwrap_or_else(|e| Value::String(e.to_string()));
        Self {
            value,
            is_error: false,
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            value: Value::String(msg.into()),
            is_error: true,
        }
    }
}

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema of the arguments.
    pub input_schema: Value,
}

fn def<A: schemars::JsonSchema>(name: &'static str, description: &'static str) -> ToolDef {
    ToolDef {
        name,
        description,
        input_schema: serde_json::to_value(schemars::schema_for!(A))
            .unwrap_or(json!({"type": "object"})),
    }
}

pub fn definitions() -> Vec<ToolDef> {
    vec![
        def::<ListArgs>(
            "calendar_list",
            "List events between two dates (local time). Defaults to the next 7 days.",
        ),
        def::<CreateArgs>(
            "calendar_create",
            "Add an event: something with a time slot. For 'remind me', use reminders_create.",
        ),
        def::<UpdateArgs>(
            "calendar_update",
            "Change an event. Get its uid from calendar_list.",
        ),
        def::<UidArgs>(
            "calendar_delete",
            "Delete an event. Confirm with the user first unless they asked for it explicitly.",
        ),
        def::<RemindArgs>(
            "reminders_create",
            "Remind the user of something at a given time.",
        ),
        def::<RemindersListArgs>("reminders_list", "List pending reminders, soonest first."),
        def::<UidArgs>("reminders_done", "Mark a reminder as done."),
        def::<UidArgs>("reminders_delete", "Delete a reminder."),
    ]
}

/// Run the tool `name` with `arguments` (JSON text).
pub async fn call<B: CalendarBackend>(b: &B, tz: Tz, name: &str, arguments: &str) -> Output {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(e) => return Output::error(format!("invalid arguments: {e}")),
    };
    match name {
        "calendar_list" => with(args, |a| calendar_list(b, tz, a)).await,
        "calendar_create" => with(args, |a| calendar_create(b, tz, a)).await,
        "calendar_update" => with(args, |a| calendar_update(b, tz, a)).await,
        "calendar_delete" => with(args, |a| calendar_delete(b, a)).await,
        "reminders_create" => with(args, |a| reminders_create(b, tz, a)).await,
        "reminders_list" => with(args, |a| reminders_list(b, tz, a)).await,
        "reminders_done" => with(args, |a| reminders_done(b, tz, a)).await,
        "reminders_delete" => with(args, |a| reminders_delete(b, a)).await,
        other => Output::error(format!("unknown tool {other}")),
    }
}

/// Deserialize the arguments into `A`, explaining where they do not fit, then run `f`.
async fn with<A: DeserializeOwned, Fut: Future<Output = Output>>(
    args: Value,
    f: impl FnOnce(A) -> Fut,
) -> Output {
    match serde_path_to_error::deserialize::<_, A>(args) {
        Ok(a) => f(a).await,
        Err(e) => Output::error(format!(
            "invalid arguments at `{}`: {}; fix them to match the tool's input schema",
            e.path(),
            e.inner()
        )),
    }
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
    if ok {
        Ok(())
    } else {
        Err("end must be after start".into())
    }
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

fn not_found(kind: &str, uid: &str, list: &str) -> Output {
    Output::error(format!(
        "no {kind} with uid {uid:?}; call {list} to get current uids"
    ))
}

async fn calendar_list<B: CalendarBackend>(b: &B, tz: Tz, a: ListArgs) -> Output {
    let from = match a.from.as_deref().map(|s| parse_local(s, tz)) {
        None => midnight(Utc::now().with_timezone(&tz).date_naive(), tz),
        Some(Ok(w)) => sort_key(&w, tz),
        Some(Err(e)) => return Output::error(e),
    };
    let to = match a.to.as_deref().map(|s| parse_local(s, tz)) {
        None => from + Duration::days(7),
        Some(Ok(When::Date(d))) => midnight(d + Days::new(1), tz),
        Some(Ok(When::At(t))) => t,
        Some(Err(e)) => return Output::error(e),
    };
    match b.list_events(from, to).await {
        Ok(mut events) => {
            events.sort_by_key(|e| sort_key(&e.start, tz));
            Output::ok(events.iter().map(|e| event_json(e, tz)).collect::<Vec<_>>())
        }
        Err(e) => Output::error(format!("{e:#}")),
    }
}

async fn calendar_create<B: CalendarBackend>(b: &B, tz: Tz, a: CreateArgs) -> Output {
    let start = match parse_local(&a.start, tz) {
        Ok(s) => s,
        Err(e) => return Output::error(e),
    };
    let end = match a.end.as_deref().map(|e| parse_end(&start, e, tz)) {
        None => default_end(&start),
        Some(Ok(e)) => e,
        Some(Err(e)) => return Output::error(e),
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
        Ok(()) => Output::ok(event_json(&event, tz)),
        Err(e) => Output::error(format!("{e:#}")),
    }
}

async fn calendar_update<B: CalendarBackend>(b: &B, tz: Tz, a: UpdateArgs) -> Output {
    let mut e = match b.get_event(&a.uid).await {
        Ok(Some(e)) => e,
        Ok(None) => return not_found("event", &a.uid, "calendar_list"),
        Err(e) => return Output::error(format!("{e:#}")),
    };
    let old_start = e.start;
    if let Some(s) = &a.start {
        match parse_local(s, tz) {
            Ok(s) => e.start = s,
            Err(err) => return Output::error(err),
        }
    }
    e.end = match (&a.end, &a.start, e.end) {
        (Some(end), _, _) => match parse_end(&e.start, end, tz) {
            Ok(end) => Some(end),
            Err(err) => return Output::error(err),
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
        return Output::error(err);
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
        Ok(()) => Output::ok(event_json(&e, tz)),
        Err(err) => Output::error(format!("{err:#}")),
    }
}

async fn calendar_delete<B: CalendarBackend>(b: &B, a: UidArgs) -> Output {
    match b.delete_event(&a.uid).await {
        Ok(true) => Output::ok(json!({"deleted": a.uid})),
        Ok(false) => not_found("event", &a.uid, "calendar_list"),
        Err(e) => Output::error(format!("{e:#}")),
    }
}

async fn reminders_create<B: CalendarBackend>(b: &B, tz: Tz, a: RemindArgs) -> Output {
    let due = match reminder_time(&a.at, tz) {
        Ok(t) => t,
        Err(e) => return Output::error(e),
    };
    let r = Reminder {
        uid: uuid::Uuid::new_v4().to_string(),
        text: a.text,
        due,
        done: false,
    };
    match b.put_reminder(&r).await {
        Ok(()) => Output::ok(reminder_json(&r, tz)),
        Err(e) => Output::error(format!("{e:#}")),
    }
}

async fn reminders_list<B: CalendarBackend>(b: &B, tz: Tz, a: RemindersListArgs) -> Output {
    match b.list_reminders().await {
        Ok(mut rs) => {
            if !a.include_done.unwrap_or(false) {
                rs.retain(|r| !r.done);
            }
            rs.sort_by_key(|r| r.due);
            Output::ok(rs.iter().map(|r| reminder_json(r, tz)).collect::<Vec<_>>())
        }
        Err(e) => Output::error(format!("{e:#}")),
    }
}

async fn reminders_done<B: CalendarBackend>(b: &B, tz: Tz, a: UidArgs) -> Output {
    match b.get_reminder(&a.uid).await {
        Ok(Some(mut r)) => {
            r.done = true;
            match b.put_reminder(&r).await {
                Ok(()) => Output::ok(reminder_json(&r, tz)),
                Err(e) => Output::error(format!("{e:#}")),
            }
        }
        Ok(None) => not_found("reminder", &a.uid, "reminders_list"),
        Err(e) => Output::error(format!("{e:#}")),
    }
}

async fn reminders_delete<B: CalendarBackend>(b: &B, a: UidArgs) -> Output {
    match b.delete_reminder(&a.uid).await {
        Ok(true) => Output::ok(json!({"deleted": a.uid})),
        Ok(false) => not_found("reminder", &a.uid, "reminders_list"),
        Err(e) => Output::error(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::block_on;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeBackend {
        events: Mutex<Vec<Event>>,
        reminders: Mutex<Vec<Reminder>>,
    }

    impl CalendarBackend for FakeBackend {
        async fn list_events(
            &self,
            from: DateTime<Utc>,
            to: DateTime<Utc>,
        ) -> anyhow::Result<Vec<Event>> {
            Ok(self
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| match e.start {
                    When::At(t) => t >= from && t < to,
                    When::Date(_) => true,
                })
                .cloned()
                .collect())
        }
        async fn get_event(&self, uid: &str) -> anyhow::Result<Option<Event>> {
            Ok(self
                .events
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.uid == uid)
                .cloned())
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
            Ok(self
                .reminders
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.uid == uid)
                .cloned())
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

    /// The tools over an in-memory calendar, in the Paris time zone.
    #[derive(Default)]
    struct Tools(FakeBackend);

    impl Tools {
        fn call(&self, name: &str, args: Value) -> Output {
            block_on(call(
                &self.0,
                chrono_tz::Europe::Paris,
                name,
                &args.to_string(),
            ))
        }
    }

    fn setup() -> Tools {
        Tools::default()
    }

    fn text(out: &Output) -> String {
        out.value.to_string()
    }

    #[test]
    fn create_then_list_in_local_time() {
        let reg = setup();
        let out = reg.call(
            "calendar_create",
            json!({"title": "Dentiste", "start": "2026-11-20T10:00", "location": "Lyon"}),
        );
        assert!(!out.is_error, "{}", text(&out));
        let uid = out.value["uid"].as_str().unwrap().to_string();
        let list = reg.call(
            "calendar_list",
            json!({"from": "2026-11-20", "to": "2026-11-20"}),
        );
        assert_eq!(list.value[0]["uid"], uid);
        assert_eq!(list.value[0]["start"], "2026-11-20T10:00+01:00");
        assert_eq!(list.value[0]["end"], "2026-11-20T11:00+01:00");
        assert_eq!(list.value[0]["location"], "Lyon");
        let none = reg.call(
            "calendar_list",
            json!({"from": "2026-11-21", "to": "2026-11-22"}),
        );
        assert_eq!(none.value, json!([]));
    }

    #[test]
    fn end_defaults() {
        let reg = setup();
        reg.call(
            "calendar_create",
            json!({"title": "Vacances", "start": "2026-12-24"}),
        );
        reg.call(
            "calendar_create",
            json!({"title": "Pont", "start": "2026-12-28", "end": "2026-12-30"}),
        );
        let list = reg.call(
            "calendar_list",
            json!({"from": "2026-12-01", "to": "2026-12-31"}),
        );
        let ev: Vec<_> = list
            .value
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["title"].as_str().unwrap(),
                    e["start"].as_str().unwrap(),
                    e["end"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            ev,
            vec![
                ("Vacances", "2026-12-24", "2026-12-24"),
                ("Pont", "2026-12-28", "2026-12-30")
            ]
        );
    }

    #[test]
    fn update_keeps_unset_fields() {
        let reg = setup();
        let out = reg.call(
            "calendar_create",
            json!({"title": "Dentiste", "start": "2026-11-20T10:00", "location": "Lyon"}),
        );
        let uid = out.value["uid"].as_str().unwrap();
        let up = reg.call(
            "calendar_update",
            json!({"uid": uid, "start": "2026-11-20T15:00"}),
        );
        assert!(!up.is_error, "{}", text(&up));
        let list = reg.call(
            "calendar_list",
            json!({"from": "2026-11-20", "to": "2026-11-20"}),
        );
        assert_eq!(list.value[0]["title"], "Dentiste");
        assert_eq!(list.value[0]["location"], "Lyon");
        assert_eq!(list.value[0]["start"], "2026-11-20T15:00+01:00");
        assert_eq!(
            list.value[0]["end"], "2026-11-20T16:00+01:00",
            "duration is kept when only start moves"
        );
    }

    #[test]
    fn unknown_uid_explains() {
        let reg = setup();
        for (tool, hint) in [
            ("calendar_update", "calendar_list"),
            ("calendar_delete", "calendar_list"),
            ("reminders_done", "reminders_list"),
            ("reminders_delete", "reminders_list"),
        ] {
            let out = reg.call(tool, json!({"uid": "nope"}));
            assert!(
                out.is_error && text(&out).contains(hint),
                "{tool}: {}",
                text(&out)
            );
        }
    }

    #[test]
    fn reminders_sorted_and_done_hidden() {
        let reg = setup();
        reg.call(
            "reminders_create",
            json!({"text": "B", "at": "2026-11-20T18:00"}),
        );
        let a = reg.call(
            "reminders_create",
            json!({"text": "A", "at": "2026-11-19T08:00"}),
        );
        reg.call("reminders_create", json!({"text": "C", "at": "2026-11-21"}));
        let list = reg.call("reminders_list", json!({}));
        let texts: Vec<_> = list
            .value
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["text"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(texts, vec!["A", "B", "C"]);
        assert_eq!(
            list.value[2]["at"], "2026-11-21T09:00+01:00",
            "date alone means 09:00"
        );
        reg.call("reminders_done", json!({"uid": a.value["uid"]}));
        assert_eq!(
            reg.call("reminders_list", json!({}))
                .value
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            reg.call("reminders_list", json!({"include_done": true}))
                .value
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn bad_date_explains() {
        let reg = setup();
        let out = reg.call("calendar_create", json!({"title": "X", "start": "demain"}));
        assert!(out.is_error && text(&out).contains("YYYY-MM-DD"));
        let out = reg.call(
            "calendar_create",
            json!({"title": "X", "start": "2026-11-20T10:00", "end": "2026-11-20T09:00"}),
        );
        assert!(
            out.is_error && text(&out).contains("after"),
            "{}",
            text(&out)
        );
    }

    #[test]
    fn bad_arguments_point_to_the_field() {
        let reg = setup();
        let out = reg.call(
            "calendar_create",
            json!({"title": 3, "start": "2026-11-20"}),
        );
        let t = text(&out);
        assert!(
            out.is_error && t.contains("`title`") && t.contains("expected"),
            "{t}"
        );
        let out = reg.call("calendar_nope", json!({}));
        assert!(out.is_error && text(&out).contains("unknown tool"));
    }

    #[test]
    fn tool_definitions_match_the_native_ones() {
        let defs = definitions();
        assert_eq!(
            defs.iter().map(|d| d.name).collect::<Vec<_>>(),
            [
                "calendar_list",
                "calendar_create",
                "calendar_update",
                "calendar_delete",
                "reminders_create",
                "reminders_list",
                "reminders_done",
                "reminders_delete"
            ]
        );
        let create = &defs[1].input_schema;
        assert_eq!(create["title"], "CreateArgs");
        assert_eq!(create["required"], json!(["title", "start"]));
        assert_eq!(
            create["properties"]["end"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            defs[4].input_schema["properties"]["at"]["description"],
            "YYYY-MM-DDTHH:MM (local); a date alone means 09:00."
        );
    }
}

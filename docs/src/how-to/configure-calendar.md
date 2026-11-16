# Configure the calendar

Fronde reads and writes events and reminders on a CalDAV server: Nextcloud, Radicale, Fastmail
and others. Events go to one calendar, reminders to a task list (`VTODO`).

## 1. Find the two collection URLs

You need the address of the calendar for events and of the task list for reminders. On Nextcloud
they look like:

```text
https://cloud.example/remote.php/dav/calendars/<user>/personal/
https://cloud.example/remote.php/dav/calendars/<user>/tasks/
```

Your calendar app usually shows them under the calendar's settings, as the CalDAV or "internal"
link. Both may be the same collection if your server keeps events and tasks together.

## 2. Add the section

```toml
[calendar]
events_url = "https://cloud.example/remote.php/dav/calendars/alex/personal/"
reminders_url = "https://cloud.example/remote.php/dav/calendars/alex/tasks/"
username = "alex"
password = { env = "FRONDE_CALDAV_PASSWORD" }
```

Fronde signs in with HTTP Basic authentication. Prefer an app password to your account password.
Run `fronde check --config fronde.toml`: it fails if the password's file or variable is missing.

## 3. Restart and try

Restart Fronde, then ask *Qu'est-ce que j'ai cette semaine ?* or *Rappelle-moi d'appeler le
garage demain à 10 h*. The calendar tools appear only when `[calendar]` is set. See
[Tools](../reference/tools.md) for what each one does.

## What to expect

- Times are read and written in `[data] timezone`.
- Recurring events and tasks show in lists, but Fronde does not change or delete them: it says to
  do it in your calendar app.
- Fronde edits stored events in place and keeps the properties it does not model, so details
  added by other apps survive.
- Fronde does not notify you of reminders itself: your task app does, from the due time.

## Planned: CalDAV as a Wasm component

The CalDAV client is moving out of the hub into a WebAssembly component, so that the hub holds
the CalDAV password and adds it to the component's requests, and the component itself only
reaches the server's origin. This is **planned**: today the hub talks CalDAV itself, and
`[calendar]` above is the way to configure it. This page will change when the component ships.

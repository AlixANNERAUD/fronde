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

## 2. Add the calendar component

The calendar is a component: `calendar.wasm`, with its manifest `calendar.toml` beside it. The
container image has both in `/app/components`; from a checkout, `mise run components` writes them
to `components/dist`. Add a `[[sources]]` entry:

```toml
[[sources]]
kind = "component"
name = "calendar"
path = "/app/components/calendar.wasm"
allow = ["https://cloud.example"]

[sources.settings]
events_url = "https://cloud.example/remote.php/dav/calendars/alex/personal/"
reminders_url = "https://cloud.example/remote.php/dav/calendars/alex/tasks/"

[[sources.credentials]]
origin = "https://cloud.example"
basic = { username = "alex", password = { env = "FRONDE_CALDAV_PASSWORD" } }
```

`allow` holds the origin (`scheme://host[:port]`) of each URL, once; add the second one if the
two collections are on different servers. The component reaches nothing else. Fronde signs in
with HTTP Basic authentication for that origin; the component never sees the password. Prefer an
app password to your account password. The time zone comes from `[data] timezone`.

A server on this machine or on your local network is refused unless its origin is allowed as
local, `{ origin = "http://192.168.1.10:5232", local = true }`. Fronde sends the password over
plain http only to this machine (`localhost`, `127.0.0.1`): a server elsewhere on the network
needs https.

## 3. Approve it

Run `fronde check --config fronde.toml`. It prints two lines:

```text
source calendar: sha256 = "…"
source calendar: manifest_sha256 = "…"
```

and fails because the component is not approved yet. Read `calendar.toml`: it says which origins
and credentials the component asks for. Then copy both lines' values into the entry, as
`sha256 = "…"` and `manifest_sha256 = "…"`, and run `fronde check` again: it also fails if the
password's file or variable is missing. After an update of Fronde or of the component, the
hashes change and the calendar is skipped, with a warning in the log, until you approve the new
ones the same way.

## 4. Restart and try

Restart Fronde, then ask *Qu'est-ce que j'ai cette semaine ?* or *Rappelle-moi d'appeler le
garage demain à 10 h*. The calendar tools appear only when the component loads; if it does not,
the log says why. See [Tools](../reference/tools.md) for what each one does.

## Coming from `[calendar]`

Earlier versions had a `[calendar]` section, which Fronde now refuses. Move its values:

| `[calendar]` | `[[sources]]` entry |
| --- | --- |
| `events_url`, `reminders_url` | `[sources.settings]`, unchanged |
| `username`, `password` | `[[sources.credentials]]`, `basic = { username = …, password = … }`, with the origin of the URLs |
| (none) | `allow`: the origin of the URLs |

## What to expect

- Times are read and written in `[data] timezone`.
- Windows time-zone names, which Outlook and Exchange write (`Romance Standard Time`,
  `Pacific Standard Time`), are understood as the matching IANA zone.
- Recurring events and tasks show in lists, but Fronde does not change or delete them: it says to
  do it in your calendar app.
- Edits keep every property and parameter Fronde does not model (alarms, attendees, time zones,
  attachments, custom fields), though line wrapping, parameter order or quoting may change. An
  item with an inline attachment in a form Fronde cannot write back exactly is not edited: it
  says to do it in your calendar app.
- Fronde does not notify you of reminders itself: your task app does, from the due time.
- An item no tool can read is left out of lists, with a `skipping unreadable event` warning in
  the log.
- A server that refuses the user name or password gets the agent to ask you to check the
  credentials entry.

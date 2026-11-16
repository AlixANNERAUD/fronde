# Component manifest

Each component project has a `fronde-component.toml` beside its `Cargo.toml`. It says what the
component is, which version of the [component interface](wit-interface.md) it targets, and what it
asks for: the origins it reaches, the credentials the host should add, tighter limits and its
settings. The manifest is a request, never a grant: Fronde's configuration decides what a
component gets, and never more than its manifest asks.

Fronde reads it strictly: an unknown key, a wildcard in the wrong place or a setting that looks
like a secret is an error.

```toml
schema = 1
id = "fronde/calendar"
version = "0.1.0"
kind = "component"
world = "tool"
api = "0.1"
description = "Calendar and reminder tools over CalDAV."
license = "Apache-2.0"
repository = "https://github.com/AlixANNERAUD/fronde"

[permissions]
network = ["{{ events_url }}", "{{ reminders_url }}"]

[[credentials]]
name = "caldav"
injection = "basic"
origins = ["{{ events_url }}", "{{ reminders_url }}"]

[settings]
type = "object"
additionalProperties = false
required = ["events_url", "reminders_url"]

[settings.properties.events_url]
type = "string"
format = "uri"
```

## Identity

| Key | Required | Value |
| --- | --- | --- |
| `schema` | yes | The manifest format: `1`. |
| `id` | yes | Never changes once published. Reverse-DNS (`org.example.weather`) or `publisher/name` (`fronde/calendar`), in lower-case letters, digits and hyphens. |
| `version` | yes | The component's version, [semver](https://semver.org). |
| `kind` | yes | `"component"`. |
| `world` | yes | `"tool"`, the only world so far. |
| `api` | yes | The `fronde:component` version, `major.minor`: `"0.1"`. Fronde refuses one it does not serve. |
| `description` | yes | One sentence. |
| `license` | yes | An SPDX expression, such as `"MIT"`. |
| `authors` | no | A list of names. |
| `repository` | no | A URL. |

## `[permissions]`

`network` lists the origins the component asks to reach, in the syntax of Spin's
`allowed_outbound_hosts`, restricted:

| Entry | Means |
| --- | --- |
| `"https://api.example.com"` | That origin; the port is the scheme's unless given (`:8443`). |
| `"https://*.example.com"` | Any subdomain of `example.com`, not `example.com` itself. `*` may only be the first label of the host: not alone, not in the scheme or port, not inside a name, not over a top-level domain or an address. |
| `"{{ events_url }}"` | The origin of the URL the user gives in that setting, which must be declared with `format = "uri"`. Unlike Spin, a setting stands for a whole entry, never part of one. |

Only `http` and `https` exist; no path, user or query. Hosts are public names: an address
(`127.0.0.1`, `[::1]`) or `localhost` (and `*.localhost`) is refused. A server of the user's, on
the machine or its network, is reached through a URL setting whose origin the configuration
allows as local.

## `[[credentials]]`

A secret the host adds to the component's requests for some of its origins. The component never
sees it.

| Key | Value |
| --- | --- |
| `name` | Unique in the manifest: lower-case letters, digits, `_` and `-`. |
| `injection` | `"basic"` (`Authorization: Basic`, from a user name and password), `"bearer"` (`Authorization: Bearer`), or `"header:<Name>"` (the secret as that header's value; hop-by-hop headers and `Host` are refused). |
| `origins` | Entries of `permissions.network` the credential goes to. |

## `[limits]`

Optional, each within the host's limit: `memory_mb` (up to 64), `call_timeout_ms` (up to
60000), `cpu_ms` (running time, up to 10000) and `request_timeout_ms` (up to 30000).

## `[settings]`

The [JSON Schema](https://json-schema.org) of the component's settings, written in TOML. It is
an object with `additionalProperties = false`, whose properties are all strings (`type =
"string"`, with optional `title`, `description`, `format`, `default`, `enum`, `pattern` and
`examples`). Setting names match `[a-z][a-z0-9_]*`.

Settings reach the component, so they never hold a secret: a property whose name contains a
word such as `password`, `secret`, `token` or `api_key`, or that has `writeOnly = true`,
`format = "password"` or `x-secret`, is refused. Declare a credential instead.

Without `[settings]`, the component has none.

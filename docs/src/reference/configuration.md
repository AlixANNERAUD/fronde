# Configuration

Fronde reads one TOML file, given with `--config`. Unknown sections and keys are errors, so a typo
never goes unnoticed. The file is read at start: restart Fronde after a change.
`fronde check --config <file>` validates it and its secrets. The repository's
[`fronde.example.toml`](https://github.com/AlixANNERAUD/fronde/blob/main/fronde.example.toml)
shows every section.

## Relative paths

A relative path in the file (`[data] dir`, a secret `file`) is resolved from the directory
containing the configuration file, whatever directory Fronde is started from, and becomes
absolute. Earlier versions read relative secret paths from the working directory.

## Secrets

A secret is never written in the file. It is one of:

| Form | Meaning |
| --- | --- |
| `{ file = "/run/secrets/name" }` | The file's content, without its trailing line breaks. A relative path is read from the directory of the configuration file, not from Fronde's working directory. |
| `{ env = "NAME" }` | The value of the environment variable `NAME`. |

The settings below marked *secret* take one of these forms.

A secret file must be private. On Unix, Fronde refuses a file that group or others can read or
write, or that another user owns, and says so: `secret file <path> is readable by other users; run
chmod 600 <path>`. `fronde check` reports it too. For Docker secrets, which are usually mode 0444
and owned by root, set the environment variable `FRONDE_ALLOW_SHARED_SECRET_FILES=1` to skip
this check. It is off by default and applies to every secret file.

## `[server]`

Required.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `listen` | address | `"127.0.0.1:8484"` | Address and port to listen on. Use `"0.0.0.0:8484"` in a container. |
| `token` | secret | required | Bearer token for `/mcp`. At least 32 characters. Fronde's own agent uses it too. |

## `[data]`

Required.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `dir` | path | `$XDG_DATA_HOME/fronde`, else `~/.local/share/fronde` (macOS and Windows: the platform's per-user data directory) | Where Fronde keeps its memory, index and conversation. Created (mode 0700) if missing; it must be writable by the user running Fronde. A relative path is read from the configuration file's directory. See [Data directory](data-directory.md). |
| `timezone` | IANA name | required | Time zone of the journal, the calendar and reminders, such as `"Europe/Paris"`. |
| `user` | string | `"me"` | What the assistant calls you, in its instructions and its system prompt. |

## `[memory]`

Optional.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `memory_limit` | integer | `4000` | Maximum size of `MEMORY.md`, in characters. |
| `user_limit` | integer | `2000` | Maximum size of `USER.md`, in characters. |
| `git` | boolean | `true` | Commit the memory directory with git after each change. Needs `git` on the `PATH`. |

## `[calendar]`

Optional. Without it, the calendar and reminder tools do not exist. All four keys are required
when the section is present. See [Configure the calendar](../how-to/configure-calendar.md).

| Key | Type | Meaning |
| --- | --- | --- |
| `events_url` | URL | CalDAV collection for events. |
| `reminders_url` | URL | CalDAV collection for reminders (tasks). |
| `username` | string | CalDAV user name. |
| `password` | secret | CalDAV password, sent with HTTP Basic authentication. |

## `[[mcp]]`

Zero or more upstream MCP servers. See
[Add upstream MCP servers](../how-to/add-upstream-mcp-servers.md).

| Key | Type | Meaning |
| --- | --- | --- |
| `name` | string | Required. Unique, matches `^[a-z][a-z0-9_]{0,63}$`, not `hub`. Prefixes the server's tools. |
| `url` | URL | Streamable HTTP address of the server. |
| `command` | array of strings | Program and arguments to run, talking MCP over standard input and output. |
| `token` | secret | Optional, with `url` only: sent as `Authorization: Bearer <token>`. |

Each server sets exactly one of `url` or `command`.

```toml
[[mcp]]
name = "mcphub"
url = "https://mcphub.example/mcp"
token = { env = "MCPHUB_TOKEN" }
```

## `[web]`

Optional. With it, Fronde serves the chat at `/` and runs its agent. Without it, the server is
only the hub: `/health` and `/mcp`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `password` | secret | required | The chat's password. Must not be empty. |
| `secure_cookie` | boolean | `false` | Mark the session cookie `Secure`. Set it to `true` when the page is served over https. |

## `[agent]`

Optional. The agent Fronde runs for the chat, over ACP on standard input and output. It is
started on the first message, and only when `[web]` is set.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `command` | array of strings | `["claude-agent-acp"]` | Program and arguments. A bare name is looked up on the `PATH`, which does not include `agent/node_modules/.bin` outside `mise exec`: use the adapter's full path (`fronde init` writes it when it finds it). `fronde check` warns when the program is not found. |
| `env` | table of strings | `{}` | Extra environment variables, in plain text: never a secret. The agent also inherits Fronde's environment. |
| `builtin_tools` | array of strings | `["WebSearch", "WebFetch"]` | Claude Code's own tools the agent may use besides the hub's. Only `WebSearch` and `WebFetch` are allowed; `[]` for none. |

```toml
[agent]
command = ["claude-agent-acp"]
env = { CLAUDE_CODE_EXECUTABLE = "/home/alex/.local/bin/claude" }
builtin_tools = ["WebSearch", "WebFetch"]
```

## Environment variables

| Variable | Meaning |
| --- | --- |
| `FRONDE_SITE_ROOT` | Directory of the built web interface. Default `target/dx/fronde-web/release/web/public`, relative to the working directory. |
| `RUST_LOG` | Log filter, such as `debug` or `fronde_hub=debug`. Default `info`. |
| `CLAUDE_CODE_OAUTH_TOKEN` | Read by the agent, not by Fronde: the token from `claude setup-token`. |

# Store a secret

Fronde can keep a secret, such as a CalDAV password, encrypted in its database, bound to the one
origin it is for. You type it once into `fronde secret set`; it never goes in the configuration
file, and a component's credential names it with `{ stored = "…" }`.

## Store it

Give the secret a name (letters, digits, `-`, `_` and `.`, at most 64 characters) and the origin
it is for, and pass the value on standard input:

```sh
fronde secret set caldav --origin https://cloud.example --config fronde.toml < caldav-password
```

or, to type it without leaving it in a file or in your shell history (`read -s` does not echo,
`IFS=` keeps leading and trailing spaces, and `printf` is built into the shell, so the value
never shows in the process list):

```sh
IFS= read -rs value
printf '%s' "$value" | fronde secret set caldav --origin https://cloud.example --config fronde.toml
unset value
```

`fronde secret set` never takes the value as an argument, where other users could see it in the
process list: an extra argument is refused without being shown. It also refuses to read the
value from a terminal, which would echo it. One final line break
is dropped. The origin is stored as Fronde normalises it: `HTTPS://Cloud.Example:443` is stored as
`https://cloud.example`. Setting a name again replaces its value and origin.

The first secret creates the master key, `master.key` beside the configuration file (mode 0600),
and prints its path. It is kept out of the data directory, so that backups of the data directory
do not hold it: back it up now, once, apart from them, in a password manager for instance (see
[Data directory](../reference/data-directory.md)). Without it, the stored secrets cannot be read,
and Fronde does not make a new key while secrets are stored. To keep it elsewhere, set
`[secrets] master_key = { file = "…" }` (see
[Configuration](../reference/configuration.md#secrets-1)) before storing the first secret.

If the configuration's directory is read-only, as in a container, the key cannot be created
there: `fronde secret set` says so and names both fixes, a writable directory there or
`[secrets] master_key`. [Run in Docker](run-in-docker.md#stored-secrets) shows the second.

## Use it in a credential

Name it in the component's `[[sources.credentials]]`, for the origin you stored it for:

```toml
[[sources.credentials]]
origin = "https://cloud.example"
basic = { username = "alex", password = { stored = "caldav" } }
```

or `bearer = { stored = "api-token" }`. Then check it:

```sh
fronde check --config fronde.toml
```

which prints `source calendar: credential for https://cloud.example: stored secret caldav opens,
stored for this origin`, never the value. It reports a secret that is not stored, one stored for
another origin, and a master key that does not open the stored secrets, each with what to do.

Only a component's credential takes `{ stored = "…" }`: the `/mcp` token, the chat password and
an upstream server's token still name a file or an environment variable.

To move a password from a file into the store, store it from the file, replace
`{ file = "…" }` by `{ stored = "caldav" }`, run `fronde check`, then delete the file:

```sh
fronde secret set caldav --origin https://cloud.example --config fronde.toml < caldav-password
```

## See what is stored

```sh
fronde secret list --config fronde.toml
```

prints each secret's name, origin and last change. It never prints a value: no command does.

## Remove it

```sh
fronde secret remove caldav --config fronde.toml
```

These commands work while Fronde runs. A component waiting for a secret that is not stored yet,
or that is stored for another origin, is tried again, and starts within a minute of
`fronde secret set`. A component already running keeps the value it read when it started: after
replacing or removing a secret, restart Fronde.

# Store a secret

Fronde can keep a secret, such as a CalDAV password, encrypted in its database, bound to the one
origin it is for. You type it once into `fronde secret set`; it never goes in the configuration
file. Using a stored secret in a component's `[[sources.credentials]]` (`{ stored = "…" }`) is
**planned**: today you can store, list and remove secrets, and a credential still names a file or
an environment variable.

## Store it

Give the secret a name (letters, digits, `-`, `_` and `.`, at most 64 characters) and the origin
it is for, and pass the value on standard input:

```sh
fronde secret set caldav --origin https://cloud.example --config fronde.toml < caldav-password
```

or, to type it without leaving it in a file or in your shell history (`read -s` does not echo,
and `printf` is built into the shell, so the value never shows in the process list):

```sh
read -rs value
printf '%s' "$value" | fronde secret set caldav --origin https://cloud.example --config fronde.toml
unset value
```

`fronde secret set` never takes the value as an argument, where other users could see it in the
process list, and refuses to read it from a terminal, which would echo it. One final line break
is dropped. The origin is stored as Fronde normalises it: `HTTPS://Cloud.Example:443` is stored as
`https://cloud.example`. Setting a name again replaces its value and origin.

The first secret creates the master key, `master.key` in the data directory (mode 0600). Back it
up now, apart from the database: see [Data directory](../reference/data-directory.md). Without
it, the stored secrets cannot be read, and Fronde does not make a new key while secrets are
stored.

## See what is stored

```sh
fronde secret list --config fronde.toml
```

prints each secret's name, origin and last change. It never prints a value: no command does.

## Remove it

```sh
fronde secret remove caldav --config fronde.toml
```

These commands work while Fronde runs. A running Fronde does not use stored secrets yet.

# Store a secret

Fronde can keep a secret, such as a CalDAV password, encrypted in its database, bound to the one
origin it is for. You type it once, into `fronde secret set` or into a card in the chat; it never
goes in the configuration file, and a component's credential, an upstream MCP server's token or
the agent's sign-in token names it with `{ stored = "…" }`.

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

## Or type it in the chat

When an agent needs a secret, it asks with a card (`user_secret`) rather than in a message. Type
the value in the masked box and press *Enregistrer*:

- left as it is, the secret is kept **for this conversation only**, in memory, for an hour, and
  used once. Nothing is stored.
- when the agent proposed a name and an origin, you also see *Garder pour <origin>* (only as the
  owner). Tick it to store the secret, exactly as `fronde secret set <name> --origin <origin>`
  would: it is listed by `fronde secret list` and used as below. Check the origin on the card
  before ticking: the secret will only ever be sent there.

Never paste a secret in a chat message: that one goes through the model and the conversation.
Refuse the card if you did not expect it.

## Where the master key goes

The first secret creates the master key that seals them all, and `fronde secret set` says where
it put it (for a secret kept from a card, the closed card says *clé maîtresse créée : …*, and the
log names the place). By default (`[secrets] backend = "auto"`, see
[Configuration](../reference/configuration.md#secrets-1)):

- under a systemd service that loads the credential `fronde-master-key`, the key is that
  credential's (see
  [Run as a systemd service](run-as-a-service.md#keep-the-master-key-in-a-systemd-credential));
- on a desktop with a keychain (GNOME Keyring, KWallet, KeePassXC with Secret Service, the
  macOS Keychain, the Windows Credential Manager), the key goes there, as the entry `fronde` /
  `master-key@…`. If the keychain is locked, or waits on an unlock prompt, the command stops and
  says to unlock it and try again: it does not put the key in a file instead;
- otherwise (a server, a container, a session with no keychain to reach), it goes to
  `master.key` beside the configuration file (mode 0600), whose path it prints. That file is kept
  out of the data directory, so that backups of the data directory do not hold it: back it up
  now, once, apart from them, in a password manager for instance (see
  [Data directory](../reference/data-directory.md)). To keep it elsewhere, set
  `[secrets] master_key = { file = "…" }` before storing the first secret.

Fronde remembers where the key went, and for the keychain the entry's name, and looks only there
from then on, even if the data directory moves. Without the key, the stored secrets cannot be
read, and Fronde does not make a new key while secrets are stored. If the keychain is locked when
Fronde starts, it stops and says to unlock it.

To choose instead of letting `auto` choose, set `backend` before storing the first secret, for
instance `backend = "file"` for a service that starts at boot, before any desktop session unlocks
the keychain, or on a machine whose keychain asks to be unlocked in a session you do not see.

## Back up a key kept in the keychain

The keychain is backed up with your home directory, but only as long as you keep its password,
and only on that machine's keychain. Keep a copy of the key line in a password manager as well.
On Linux, `secret-tool` (from libsecret) prints it, without a final line break:

```sh
secret-tool search service fronde            # the entries Fronde made, with their user names
secret-tool lookup service fronde username master-key@…
```

Copy the line, `fronde-key-v1:` and what follows, into the password manager, without saving it
in a file or your shell history. To put it back after a loss, read it from the password manager
into the entry Fronde names in its error:

```sh
secret-tool store --label='Fronde master key' service fronde username master-key@…
```

which asks for the line. On macOS, Keychain Access shows the item `fronde`; on Windows, the
Credential Manager lists it under Windows Credentials, among the generic credentials.

If the configuration's directory is read-only, as in a container, the key file cannot be created
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

A component's credential, the `token` of an upstream MCP server reached by `url` (see
[Add upstream MCP servers](add-upstream-mcp-servers.md#keep-the-token-in-the-encrypted-store);
`fronde check` prints `source up: token: stored secret … opens, stored for this origin`) and
`[agent] oauth_token` take `{ stored = "…" }`. The `/mcp` token, the chat password and the token
of a server run as a command still name a file or an environment variable.

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

## Rotate the master key

To replace the master key with a new one, for instance after a copy of it may have leaked, stop
Fronde, then run:

```sh
fronde secret rotate-key --config fronde.toml
```

It makes a new key, seals every stored secret again with it, puts it where the old one was (the
key file, or the keychain entry), and prints how many secrets it sealed again. It never prints a
key. Back up the new key as you did the first one: the old one opens nothing now. A key kept in
a systemd credential is not rotated in place, since Fronde never writes one: move it to a file,
rotate it there, then make the credential again from the new file.

The command refuses to run while Fronde runs on the same data directory, and names the process
(`process 4242 (fronde serve)`): stop the service (`systemctl --user stop fronde`) first. If the
command is interrupted, by a crash or a power cut, nothing is lost: the next `fronde serve`, or
the next `rotate-key`, finishes it with the key that opens the stored secrets, and the log says
so.

## Move the master key

To keep the same key elsewhere, for instance in the keychain after storing secrets on a machine
without one, stop Fronde, set `[secrets] backend = "auto"` (or the new backend), then run:

```sh
fronde secret move-key --to keychain --config fronde.toml
```

or `--to file`. It copies the key to the new place, reads it back, checks that it opens the stored
secrets, records the new place, and only then removes the key from the old one. It prints both
places, never the key. If the keychain is locked or cannot be reached, or already holds another
key in that entry, it stops and changes nothing. `--to systemd` explains how to make the
credential from the key file with `systemd-creds encrypt` (see
[Run as a systemd service](run-as-a-service.md#keep-the-master-key-in-a-systemd-credential)):
Fronde never writes one. Moving a key out of a systemd credential works under a unit that loads
it; Fronde then leaves the credential in place, for you to remove from the unit.

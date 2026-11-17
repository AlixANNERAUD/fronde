# Set up an integration from the chat

Some components ship with Fronde (the calendar is one). Instead of writing their `[[sources]]`
entry by hand (see [Configure the calendar](configure-calendar.md)), you can let the assistant ask
for it: you fill one card in the chat, and Fronde adds the entry to `fronde.toml` and starts it.

Only an admin (today the owner) can answer that card, and only a component shipped with this
build of Fronde, with the very files the build approves, that has no `[[sources]]` entry yet can
be set up this way. Anything else needs a hand-written entry.

## Ask for it

Tell the assistant what you want, for example *"Ajoute mon agenda Nextcloud"*. When no tool does
it yet, `search_tools` lists the components that are *available, not set up*, and the assistant
calls `integrations_request_setup` with the component's name. It may fill in the settings it
knows, such as the URL you gave it; it never fills in a password.

## Fill in the card

The card *Ajouter l'intégration « calendar »* shows:

- every setting of the component, as its manifest describes it, with what the assistant suggested,
  each such value marked *Suggéré par l'agent*; change any of them. Check a suggested URL above
  all: the password goes to that server;
- for each credential, a user name and a masked box for the password or token. What you type there
  is sealed in `fronde.db` and never shown to the assistant, the conversation or the log;
- which origins the component may reach, and where each password goes: *envoyé seulement à
  https://cloud.example*. They follow from the URLs you give;
- *Il remplace le secret « calendar_caldav » déjà gardé pour https://cloud.example*, when a secret
  is stored under the name the setup keeps it under, for the same server (from an earlier setup
  whose entry you removed, say): keeping the new password replaces it;
- *Serveur sur cette machine ou son réseau local*, unticked. Tick it only for a server at a local
  address: Fronde refuses local addresses otherwise. A card allows them only for a host that
  looks local: `localhost` or another loopback address, a private address (`192.168…`, `10…`,
  `172.16…` to `172.31…`, `fd…` in IPv6), a name without a dot (`nas`), or one ending in `.lan`,
  `.local` or `.home.arpa`. Ticked for any other host (`nas.example.com`, even if it resolves to
  a local address), the setup is refused, saying why: write that entry by hand, with
  `{ origin = "…", local = true }`. That goes for a server reached over Tailscale too, by its
  `100.64…` to `100.127…` address or its `*.ts.net` name, and for any other host that does not
  look local.

Press *Ajouter*. A second card asks you to confirm before anything is kept when you changed a
URL so that a password would go to another server than the card said, when you ticked the local
box (it lists each origin it allows as local), or when a stored secret would be replaced that the
first card did not name.

## What Fronde does

1. It checks the entry against the component's manifest with a dry run, as a start would: the
   settings, the origins (each one the manifest asks for), and local addresses.
2. It reads `fronde.toml` again and checks the entry could be added to it (see below), before
   it keeps any secret: an entry you added by hand meanwhile leaves every stored secret as it
   was.
3. It seals each password for the origins of its URL settings (`calendar_caldav`, for example),
   as `fronde secret set` would store it. The first secret kept makes the master key: back it up
   (see [Store a secret](store-a-secret.md)).
4. It checks the whole entry again with the stored secrets, then adds it at the end of
   `fronde.toml`, with a comment above it. The rest of the file, comments included, stays as it
   was, and so does its mode. The password appears only as `{ stored = "calendar_caldav" }`.
5. It starts the component; the assistant gets the names of its tools and can use them at once.
   It runs until Fronde stops, even if its entry leaves `fronde.toml` meanwhile (one you were
   given to paste, or one you removed by hand): it is not offered again until then.

The dry runs take only the component's files as the build ships them: changed on disk, they are
refused, even if the entry would pin their new hashes or you approved them.

Fronde reads `fronde.toml` again just before writing it. If you added an entry of that name by
hand meanwhile, or if the file, as it is or with the new entry, would no longer start Fronde
(every check of `config::load`, which `fronde check` runs too), nothing is written and the setup
says so. The assistant is never shown the file's content, which may hold a secret: only that it
no longer loads, and that `fronde check` says why; the log names the file and the line. If the
file changes while Fronde writes it, Fronde reads it again once, then gives up rather than lose
your edit. Fronde tells a change by the file's inode, modification time and size: an edit in
place that keeps the size, within the precision of the file system's timestamps, goes
unnoticed, and so does one in the instant before the file is replaced. The file is replaced by a
new one (an atomic rename): a hard link to it keeps the old content, so edit `fronde.toml`
itself, not a hard link to it.

Two setups of the same integration answered at once never store over each other: the first sets
it up, the other is refused before it keeps anything. A secret stored under the same name
meanwhile (by another card or `fronde secret set`) is never replaced unless the card said so.

If the assistant's call is cut (its client gives up), the setup stops at the next step. Cut
before its secrets are kept, or while it waits for another setup of the same integration,
nothing is kept or written. Cut after, nothing is written, and the secrets it stored under new
names are removed; a secret it replaced (one the card named) keeps the password you typed, the
one before is gone.

## When `fronde.toml` is read-only

In Docker the configuration is mounted read-only (`/config/fronde.toml:ro`), and Fronde never
writes through a link. Then a card *fronde.toml ne peut pas être écrit* shows the entry to paste
at the end of your `fronde.toml`. It holds no secret, only the stored names. The component runs
until Fronde restarts; paste the entry to keep it.

The master key goes beside the configuration by default, and a read-only `/config` cannot hold
it. A container has no keychain: before setting up an integration with a password there, mount a
writable directory at `/keys` and set `[secrets] master_key = { file = "/keys/master.key" }` (see
[Run in Docker](run-in-docker.md)).

## Limits

- Only the assistant of a thread you talk to may ask, during a turn your message started. A
  sub-agent cannot ask, whatever its turn: setting up an integration is an admin's change, and a
  sub-agent follows its task, not your words; it says what it needs, and the assistant that
  started it asks. A turn a sub-agent's result started cannot ask either.
- An outside MCP client cannot ask, and `search_tools` lists the integrations neither to it nor
  to a sub-agent.
- The setup needs the chat with its threads (`[web]`): without them, the tool is not served.
- A card left unanswered expires (10 minutes by default) and a restart expires it: the setup is
  not resumed later.
- A component that asks for any subdomain of a domain, or sends a credential in a header, is set
  up by hand.

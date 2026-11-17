# Set up an integration from the chat

Some components ship with Fronde (the calendar is one). Instead of writing their `[[sources]]`
entry by hand (see [Configure the calendar](configure-calendar.md)), you can let the assistant ask
for it: you fill one card in the chat, and Fronde adds the entry to `fronde.toml` and starts it.

Only an admin (today the owner) can answer that card, and only a component shipped with this
build of Fronde that has no `[[sources]]` entry yet can be set up this way. Anything else needs a
hand-written entry.

## Ask for it

Tell the assistant what you want, for example *"Ajoute mon agenda Nextcloud"*. When no tool does
it yet, `search_tools` lists the components that are *available, not set up*, and the assistant
calls `integrations_request_setup` with the component's name. It may fill in the settings it
knows, such as the URL you gave it; it never fills in a password.

## Fill in the card

The card *Ajouter l'intégration « calendar »* shows:

- every setting of the component, as its manifest describes it, with what the assistant suggested;
  change any of them;
- for each credential, a user name and a masked box for the password or token. What you type there
  is sealed in `fronde.db` and never shown to the assistant, the conversation or the log;
- which origins the component may reach, and where each password goes: *envoyé seulement à
  https://cloud.example*. They follow from the URLs you give;
- *Serveur sur cette machine ou son réseau local*, unticked. Tick it only for a server at a local
  address (`localhost`, `192.168…`, `nas.lan`): Fronde refuses local addresses otherwise.

Press *Ajouter*. If you changed a URL so that a password would go to another server than the card
said, a second card asks you to confirm the new origins before anything is kept.

## What Fronde does

1. It checks the entry against the component's manifest with a dry run, as a start would: the
   settings, the origins (each one the manifest asks for), and local addresses.
2. It seals each password for the origins of its URL settings (`calendar_caldav`, for example),
   as `fronde secret set` would store it. The first secret kept makes the master key: back it up
   (see [Store a secret](store-a-secret.md)).
3. It checks the whole entry again with the stored secrets, then adds it at the end of
   `fronde.toml`, with a comment above it. The rest of the file, comments included, stays as it
   was, and so does its mode. The password appears only as `{ stored = "calendar_caldav" }`.
4. It starts the component; the assistant gets the names of its tools and can use them at once.

## When `fronde.toml` is read-only

In Docker the configuration is mounted read-only (`/config/fronde.toml:ro`), and Fronde never
writes through a link. Then a card *fronde.toml ne peut pas être écrit* shows the entry to paste
at the end of your `fronde.toml`. It holds no secret, only the stored names. The component runs
until Fronde restarts; paste the entry to keep it.

The master key goes beside the configuration by default, and a read-only `/config` cannot hold
it: set `[secrets] master_key` to a file in a writable place, or use the keychain, before
setting up an integration with a password there (see [Run in Docker](run-in-docker.md)).

## Limits

- The assistant may ask only during a turn you started: a turn a sub-agent's result started
  cannot ask for it, and an outside MCP client cannot either.
- A card left unanswered expires (10 minutes by default) and a restart expires it: the setup is
  not resumed later.
- A component that asks for any subdomain of a domain, or sends a credential in a header, is set
  up by hand.

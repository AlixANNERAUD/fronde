# Sign in to the chat

Each person signs in to the chat page with their own name and password, and sees their own
conversations. This guide covers signing in, giving someone their sign-in, and what changes when
you upgrade from a version without names.

## Sign in

Open Fronde's address. The sign-in page (*Connexion*) asks for:

- *Prénom*: your name, exactly as the configuration writes it, `[users.<name>]` (or `[data] user`
  in the legacy form). Case matters: `Marie` is not `marie`. Spaces around it are ignored.
- *Mot de passe*: your own password.

Alone on your Fronde (no `[users]`, the legacy form), you may leave *Prénom* empty: the password
alone still signs you in, as before names.

A wrong name or a wrong password gets the same answer, *Prénom ou mot de passe incorrect.*, after
a second. Five mistakes in a row under one name lock that name for a minute (*Trop d'essais,
réessaie dans une minute.*); everyone else still signs in.

## Give someone their sign-in

1. Write their password to a file only Fronde reads:

   ```sh
   (umask 077 && printf '%s' 'a long passphrase' > marie.password)
   ```

2. Add them to `fronde.toml`, beside the owner (see
   [`[users.<name>]`](../reference/configuration.md#usersname)):

   ```toml
   [users.marie]
   password = { file = "marie.password" }
   ```

3. Check and restart: `fronde check --config fronde.toml`, then restart Fronde. The configuration
   is read once, at start.

4. Give them the address, their name as written in the section, and their password.

They see only their own conversations. Until each person has a conversation of their own at `/`
(**planned**), the main conversation is the owner's: a member landing there reads *Conversation
introuvable.* and starts one with *Nouvelle conversation*, or opens hers from the list. Their
agent needs a subscription of their own, `[accounts.<name>]`; without one, it does not start.

## Take someone's sign-in away

Remove their `[users.<name>]` section and restart. Their sessions are refused at once and deleted
at the start; their conversations become read-only. Someone added later under the same name
starts with none of their sessions or conversations.

## After the upgrade

Sessions from before names did not say who signed in. The first start of this version signs them
all out: everyone signs in once again, with their name. The same happens to a session whose
person was removed or renamed, and to the owner's when `[data] user` changes.

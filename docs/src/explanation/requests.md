# Requests and cards

Some things only you can decide: which of two apps to install, what to call a calendar, whether
an agent may act. Fronde has one way to ask you anything: a **request**, shown as a **card** in the
chat, answered once.

## One request, one card, one answer

A request is a row in the `requests` table of `fronde.db`. Fronde writes the row first, then shows
the card as an entry of the conversation, so replay and reconnection work for cards as they do
for messages: a page that opens later, on your phone for instance, shows the same open card.

You answer from any page where you are signed in. The page sends one message, `answer`, whatever
the card. The first answer wins: the row moves from `pending` to `answered` (or `declined`) only
if it is still pending, so two pages answering at once cannot both succeed. The other page gets
a short refusal, and the card closes everywhere when the server logs its end.

A request ends in one of four ways:

| Status | When | The card says |
| --- | --- | --- |
| `answered` | You sent an answer | *Réponse : …*, or for a secret where it was kept |
| `declined` | You pressed *Refuser* on a question or secret card | *Refusée* |
| `expired` | Its time ran out | *Expirée* (*Expirée, refusée* for a permission) |
| `cancelled` | The asker gave up (its tool call was cancelled), the assistant's turn that asked ended or was stopped, or the request could not be shown | *Annulée* |

The row is closed before the one who asked is told, and a closed request never reopens. Every
request expires: no answer means no. The row keeps who answered, when, and a short summary of the
answer, as an audit trail.

## Answers are checked

The card is a form, and the answer must fit it: only options the card offers, one pick for a
question with one answer, required questions answered, free text non-empty and at most 2000
characters. The page runs these checks before sending, to say at once what is missing, and the
server runs the same code again, from `fronde_protocol::request::check`: a page cannot slip an
option the card never offered past it. An answer that does not fit is refused to that page alone,
and the request stays as it was.

An agent that asked gets typed answers: the labels you picked, your free text and your note, each
in its own field. It never has to parse prose, and never gets a label it did not offer.

## Who asks, who answers

Each request is for one user, and only that user answers it. A request can also be marked for an
admin only, for cards that grant something lasting; today the one user, the owner, is the admin.

The hub knows who asks. Fronde's own assistant reaches `/mcp` with a token of its own, made at
each start and kept in memory: its questions are tied to its running turn and headed *Question de
Fronde*. Any other client uses `server.token`: its questions are headed with the name it gave when
it connected, quoted and marked as a client, such as *Question de « Codex » (client MCP)*, so a
card from an outside agent never passes for Fronde's own. Only the token makes a caller the
assistant: a client that calls itself `assistant` is still an outside client. Since a client
chooses its name, the hub refuses to ask the user anything for one named *Fronde* or *assistant*
(in any case), one whose name is longer than 40 characters, or one whose name holds a control
character.

A card's texts are capped so that it stays readable on a phone and small on the wire: a question
at most 300 characters, a label 60, a description 200, a preview 4000. None may hold a control
character (a preview may hold line breaks and tabs), nor a bidirectional control that would make
it read as something else. Each refusal tells the agent how to fix its call. The chat's WebSocket
takes messages of at most 64 KiB, and closes on a bigger one.

A closed card says how it ended, and who answered when it was not you.

## Permission cards

Fronde's assistant asks before it acts: each time the agent asks to use a tool (ACP's
`session/request_permission`), Fronde writes a `permission` request and shows its card, headed
*Fronde demande l'autorisation*. The card shows:

- the tool call's title, in one line of at most 200 characters, and its kind (*Lecture*,
  *Modification*, *Commande*, *Accès web*…);
- what it would do, in monospace: the command, the files and the address it names, its text (a
  plan), and for each file it would change a summary such as `notes.md : -1 +3 ligne(s)`, at
  most 4000 characters, folded after six lines;
- one button per option the agent offers, in its order, named by kind (*Autoriser une fois*,
  *Toujours autoriser*, *Refuser*, *Toujours refuser*), with the agent's own words for the option
  under the button when they say more, such as what *always* covers.

All of these texts come from the agent, which may have read them from a web page. Fronde caps
them and replaces every control and bidirectional character with `�` before the card is written,
so a command cannot hide its end behind a right-to-left override or an escape sequence, and the
page shows them as text, never markup. Options that last (`allow_always`, `reject_always`) are
shown only to an admin, and never on a tool call that switches the session's mode: there, Claude
Code's lasting options leave plan mode for a mode that asks less (auto mode, accepting edits), and
such a switch must never be one tap away, so the card offers only *Autoriser une fois* and
*Refuser*. The server refuses any option the card does not offer.

The answer is the same `answer` message as any card's, checked by the same code, and the first
one wins. The agent then gets the option chosen. It gets a refusal (the agent's *reject once*, or
a cancel if it offered none) when the card expires after **30 minutes** or is declined: no answer
means no. It gets a cancel when you press *Stop* or the turn ends: both close the turn's open
cards. A permission still pending when Fronde stops expires at the next start, since the agent's
call that waited on it is gone with the agent's process.

*Toujours autoriser* is the agent's own option, and Fronde passes it on without keeping anything.
With Claude Code, it adds an allow rule to the running agent process: it lasts until that process
stops (Fronde restarts, or a turn the agent never ended stops it), and is never loaded again,
since Fronde's sessions load no Claude Code settings. Fronde itself never remembers a grant: the
next agent process asks again.

## Secret cards

`user_secret` asks for one secret on a card of its own, never in a question card. The card shows
why it is needed, a button to the sign-in or setup page when the agent gave one (https only, named
by its host), and a masked box that neither checks spelling nor fills itself in; *Afficher* shows
what you typed, *Masquer* hides it again. *Enregistrer* sends it, *Refuser* declines.

By default the secret is kept **for this conversation**: in Fronde's memory only, never in
`fronde.db`, for the agent that asked and in this conversation only (any later turn of it, not
only the one that asked), for one hour, and used up by its first use. When the agent proposes a name and an origin, an admin also gets a box *Garder pour
<origin>*: ticked, the secret is sealed in `fronde.db` under that name, to be sent to that origin
only, as `fronde secret set` would store it. Nobody else sees the box, and the server refuses a
kept secret from anyone but an admin.

The answer travels from the page to the server on the chat's WebSocket and goes straight to the
secret store. The page empties the box once it is sent, and never shows the secret again. On the
server, the value becomes a `SecretString` as soon as the message is parsed, wiped once it is
stored. The message itself cannot be wiped: it may stay in the WebSocket library's buffer until
that memory is reused (see [Secrets in memory](security.md#secrets-in-memory)). Then the request closes: its row keeps where the secret went (`{"kept": "conversation",
"secret_ref": …}` or `{"kept": "stored", "name": …, "origin": …}`), the conversation says
*Enregistré, jamais montré à l'agent* or *Gardé pour <origin>* (with *clé maîtresse créée :
trousseau*, *fichier* or *identifiant systemd* when that secret made the master key, to back up),
and the agent gets the reference or the name. Two pages answering at once keep one secret: the first answer wins, and the second
is refused before anything is kept.

A `secret_ref` is useless outside its conversation and to another asker, and every refusal says
the same thing, so a guess learns nothing. Outside clients that give the same name share one
asker (see [Security](security.md#secrets-given-in-a-card)). Starting a new conversation will
forget its references (**planned**). No tool takes one yet (see
[Tools](../reference/tools.md#user_secret)); the stored name is what a component can use today.

## Restarts and retries

The table is the source of truth. After a restart, a request whose time ran out expires, and so
does a pending permission; one whose card never reached the conversation, because Fronde stopped
in between, is shown then; the others stay answerable. A tool call that is retried with the same `client_request_id` joins the
request it started instead of showing a second card. Secrets kept for a conversation do not
survive a restart: they live in memory only. A secret card still pending after a restart stays
answerable.

The id alone proves nothing: the assistant reuses ids from one turn to the next, and outside
clients choose their names, so two of them can share one. A retry therefore joins a request only
when it asks the very same questions of the same user, and only while it is current: for the
assistant, in the same turn; for an outside client, while the request is pending or within its
timeout. A cancelled request is never joined. Anything else is a new question: it gets a card of
its own, or an error asking for a new id while the request holding that id is still pending (the
table allows one pending request per caller and id). A new question never gets an old answer.

## Cards on a phone

Cards are built for a phone first: full width below 640 px, each option a row at least 44 px tall
that you can tap anywhere, *Envoyer* and *Refuser* full width. A question with one answer is a
radio group (arrow keys move the pick, as in any radio group); one with several answers is a list
of boxes. *Enter* sends the card, as in a form. *Autre…* reveals a text box, where the arrow keys
move the caret, and *Ajouter une précision* a note. An option's preview
shows under it on a phone and beside the options on a wider screen. Text from an agent is shown
as text, never as HTML.

## Compared with T3 Code

Fronde's question card follows T3 Code's question tool, which models already know: 1 to 4
questions, a short header, 2 to 4 options with descriptions, free text, several answers. It
changes a few things:

- The advised option is a flag, `recommended`, shown as a badge and never picked for you, rather
  than "(Recommended)" written into a label.
- Answers are typed and checked against the card, rather than returned as text.
- Any signed-in page answers, the first answer wins, rather than only the thread that asked.
- Statuses tell a refusal (`declined`) from a dismissal (`cancelled`) and a timeout (`expired`).
- A secret's reference works only in its conversation and for the asker, and the card itself
  offers to keep the secret for an origin, which only the owner can choose. A `url` must be
  given, over https; it is never guessed from the reason's text. There is no `value` argument for
  a secret already pasted in the chat: that one went through the model and the logs, and storing
  it would hide the leak.

## Planned

These use the same requests and cards, and are **not built yet**:

- **Tools that take a `secret_ref`**: integration setup from the chat, and commands that read a
  secret from a file in threads that have a shell.
- **Elicitations**: forms that Claude's `AskUserQuestion` and upstream MCP servers send, shown as
  cards; an upstream server's go to the admin.
- **Late answers**: an answer to a question whose asker is gone (after a restart, or a tool call
  cut short) reaches the assistant as a message. Today such an answer is saved but goes nowhere.
- **A tray** above the message box listing the open cards, and their count in the tab's title.

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
| `answered` | You sent an answer | *Réponse : …* |
| `declined` | You pressed *Refuser* | *Refusée* |
| `expired` | Its time ran out | *Expirée* |
| `cancelled` | The asker gave up, or the request could not be shown | *Annulée* |

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
it connected, such as *Question de Codex*, so a card from an outside agent never passes for
Fronde's own.

## Restarts and retries

The table is the source of truth. After a restart, a request whose time ran out expires; one
whose card never reached the conversation, because Fronde stopped in between, is shown then; the
others stay answerable. A tool call that is retried with the same `client_request_id` joins the
request it started instead of showing a second card.

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
of boxes. *Autre…* reveals a text box, and *Ajouter une précision* a note. An option's preview
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

## Planned

These use the same requests and cards, and are **not built yet**:

- **Secrets** (`user_secret`): one secret per card, sent to the secret store and never to the
  agent, which gets a reference instead.
- **Permissions**: the agent's requests to use a tool become cards too, expiring after 30 minutes
  with a refusal. Until then they keep their own card, answered with the same `answer` message.
- **Elicitations**: forms that Claude's `AskUserQuestion` and upstream MCP servers send, shown as
  cards; an upstream server's go to the admin.
- **Late answers**: an answer to a question whose asker is gone (after a restart, or a tool call
  cut short) reaches the assistant as a message. Today such an answer is saved but goes nowhere.
- **A tray** above the message box listing the open cards, and their count in the tab's title.

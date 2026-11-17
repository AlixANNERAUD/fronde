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
| `cancelled` | The asker gave up (its tool call was cancelled), the agent's turn that asked ended or was stopped, or the request could not be shown | *Annulée* |

The row is closed before the one who asked is told, and a closed request never reopens. Every
request expires: no answer means no. The row keeps who asked (`thread:<id>` for a thread's agent;
rows from before threads say `assistant` and read as thread 1's), who answered, when, and a short
summary of the answer, as an audit trail.

## Answers are checked

The card is a form, and the answer must fit it. A form holds up to 12 fields of four kinds:

| Field | Shows | Its answer |
| --- | --- | --- |
| Question (`choice`) | Options to pick, one or several, maybe *Autre…* and a note | The labels picked, the *Autre…* text, the note |
| Text (`text`) | One line, maybe for an e-mail address, an address (URI), a date or a date and time | The text, at most 2000 characters and within the field's own minimum and maximum length |
| Number (`number`) | A number, maybe whole, maybe between a minimum and a maximum | The number |
| Toggle (`toggle`) | A box to tick, on or off at first | `true` or `false` |

The rules: only fields the card has, each answered with its own kind of value; only options the
card offers, one pick for a question with one answer, required fields answered, free text
non-empty; a text field's length and format (`name@domain.tld`; a URI with a scheme such as
`https:`; `YYYY-MM-DD`; RFC 3339 such as `2026-11-17T14:30:00+01:00`); a number's bounds, and a
whole number where one is asked. A blank text counts as unanswered. The page runs these checks
before sending, to say at once what is missing, and the server runs the same code again, from
`fronde_protocol::request::check`: a page cannot slip an option the card never offered past it.
An answer that does not fit is refused to that page alone, and the request stays as it was.

An agent that asked gets typed answers: the labels you picked, your free text and your note, each
in its own field. It never has to parse prose, and never gets a label it did not offer.

## Who asks, who answers

Each request is for one user, and only that user answers it. A request can also be marked for an
admin only, for cards that grant something lasting; today the one user, the owner, is the admin.

The hub knows who asks. Each of Fronde's threads has its own agent, which reaches `/mcp` with a
token of its own, made when its process starts and kept in memory: its questions are tied to its
thread's running turn, appear as cards in that thread's stream, not in the main one, and are
headed *Question de Fronde*. The token alone names the thread: an argument, a header or a name
cannot put a card in another thread. Any other client uses `server.token`: its questions are headed with the name it gave when
it connected, quoted and marked as a client, such as *Question de « Codex » (client MCP)*, so a
card from an outside agent never passes for Fronde's own. An upstream MCP server's form is
headed with the server's name, marked as a server: *Question de « github » (serveur MCP)*, and is
for the admin only. On a form from an outside client or a server, a line above *Envoyer* says
where the answer goes: *Ce que tu saisis est envoyé à « github » (serveur MCP). N'y mets jamais
de mot de passe ni de code.* Only the token makes a caller one of Fronde's
agents: a client that calls itself `assistant` is still an outside client. Outside clients' cards
go to the main thread. The owner's page can answer any thread's card, by design: you answer for
yourself, whichever of your threads asked. A card shows in its own thread, and the thread list
marks a thread with a card waiting « attend ta réponse ». Since a client
chooses its name, the hub refuses to ask the user anything for one named *Fronde* or *assistant*
(in any case), one whose name is longer than 40 characters, or one whose name holds a control
character.

A card's texts are capped so that it stays readable on a phone and small on the wire: a question
at most 300 characters, a label 60, a description 200, a preview 4000. None may hold a hidden
character (a preview may hold line breaks and tabs): a control or format character (bidirectional
controls, zero-width characters, the soft hyphen), a line or paragraph separator, a tag
character, a variation selector, a private-use or unassigned code point, or any other code point
Unicode lets a renderer show as nothing. Such a character could make a text read as another.
Each refusal tells the agent how to fix its call. The server checks every card against these
rules before it writes it, whoever built it. The chat's WebSocket
takes messages of at most 64 KiB, and closes on a bigger one.

A closed card says how it ended, and who answered when it was not you.

## Permission cards

Fronde's assistant asks before it acts: each time the agent asks to use a tool (ACP's
`session/request_permission`), Fronde writes a `permission` request and shows its card, headed
*Fronde demande l'autorisation*. The card shows:

- the tool call's title, in one line of at most 200 characters, and its kind (*Lecture*,
  *Modification*, *Commande*, *Accès web*…);
- what it would do, in monospace: the command, the files and the address it names (or, for a
  tool such as an MCP server's, its arguments as JSON), its text (a plan), and for each file it
  would change a summary such as `notes.md : -1 +3 ligne(s)` with its first eight changed lines,
  each line at most 300 characters and the whole at most 4000, folded after six lines;
- one button per option the agent offers, in its order, named by kind (*Autoriser une fois*,
  *Toujours autoriser*, *Refuser*, *Toujours refuser*), with the agent's own words for the option
  under the button when they say more, such as what *always* covers.

All of these texts come from the agent, which may have read them from a web page, so the card
never lets one hide what it does:

- **Hidden characters** (the list above) become `�`, and the card then says *Caractères cachés
  remplacés par �*. A command cannot hide its end behind a right-to-left override, an escape
  sequence or a zero-width space.
- **Order.** The title and the body are shown left to right, character after character
  (`dir="ltr"`, `unicode-bidi: bidi-override`): right-to-left letters, which are not hidden,
  cannot reorder the arguments of `mv <hebrew> <hebrew>`.
- **Length.** A text too long keeps its head and its tail around `[… N caractères omis …]`, line
  by line and as a whole, and the card then says *Texte tronqué : le milieu est omis, vérifie le
  début et la fin*. Padding cannot push the end of a command or of a plan out of sight. A run of
  more than eight spaces shows as `[… N espaces …]`, and more than two blank lines as `[… N
  lignes vides …]`. Nothing is refused for being long: a long plan is legitimate, and refusing
  it would only refuse the agent.
- The page shows these texts as text, never markup.

Options are offered as follows:

- One-time options (*Autoriser une fois*, *Refuser*) as the agent gives them.
- A lasting option only to an admin, and only one whose effect Fronde knows. With the Claude
  adapter, that is the allow rule its *always* adds for the tool at hand: Read, Glob and Grep
  (reading, for the session), Bash and PowerShell (a command prefix), WebFetch (a domain), a
  network host, an MCP tool, a skill. Not on Edit, Write or NotebookEdit, where the same option
  may also switch the session to accepting every edit, and never another agent's: what it would
  grant cannot be told.
- On a tool call that switches the session's mode (leaving plan mode, entering it), only
  *Yes, manually approve edits* and *No*: every other option there may switch to a mode that asks
  less (auto mode, accepting every edit), which must never be one tap away.
- These lists belong to the agent's provider (see
  [How Fronde works](architecture.md#providers-and-models)). A provider whose options Fronde has
  not read gets no lasting option at all, and no option on a mode switch, which is then refused.
- An option id the agent gives with two different kinds is left out.

The server refuses any option the card does not offer, and the agent only ever gets one the card
offered.

The answer is the same `answer` message as any card's, checked by the same code, and the first
one wins. Only a thread's agent may ask for a permission, in its own thread. A refused answer (not yours, already
closed, not an option of the card) is logged as a warning naming the user and the request, never
the answer. The agent then gets the option chosen. It gets a refusal (the agent's *reject once*, or
a cancel if it offered none) when the card expires after **30 minutes** or is declined: no answer
means no. It gets a cancel when you press *Stop* or the turn ends: both close the turn's open
cards; a request that comes after its turn ended is refused at once, so no card outlives its
turn. When the agent's side cancels a request first, its card closes too, so a late tap is never
recorded as granted. A permission still pending when Fronde stops expires at the next start,
since the agent's call that waited on it is gone with the agent's process.

*Toujours autoriser* is the agent's own option, and Fronde passes it on without keeping anything.
With Claude Code, it adds an allow rule to the running agent process: it lasts until that process
stops (Fronde restarts, or a turn the agent never ended stops it), and is never loaded again,
since Fronde's sessions load no Claude Code settings. Fronde itself never remembers a grant: the
next agent process asks again.

## Forms from the agent

Claude asks you questions with its own tool, `AskUserQuestion`, as well as with `user_ask`.
Fronde tells the Claude adapter that it shows forms (ACP's `elicitation` capability, forms only),
so the adapter sends each `AskUserQuestion` as an ACP `elicitation/create` form instead of turning
the tool off. Fronde enables the tool in the agent's session but never allows it up front, so
each use goes through the adapter's permission check, which is where it asks. The adapter sends
other forms the same way: the choice to retry a refused request with another model, and the forms
of MCP servers it connects to (with Fronde's settings, it connects to Fronde's hub only, which
sends none).

Each form becomes a card of the running turn, headed *Question de Fronde*. Upstream MCP servers'
forms follow the same rules, on a card for the admin headed with the server's name (see
[Add upstream MCP servers](../how-to/add-upstream-mcp-servers.md#when-a-server-asks-you-something)).
The form's schema is a flat object of fields, and maps onto the card's fields:

| Schema | Card field |
| --- | --- |
| `string` with `oneOf` (titled options) or `enum` | A question with one answer |
| `array` whose `items` have `anyOf` or `enum` | A question with several answers (`maxItems: 1`: one answer) |
| `string` named `<k>_custom` beside a question `<k>`, not required | That question's *Autre…* and note |
| Any other `string` | A text field; `format` `email`, `uri`, `date`, `date-time`; `minLength`, `maxLength` |
| `number`, `integer` | A number field; `minimum`, `maximum` |
| `boolean` | A toggle |

`AskUserQuestion` therefore reads as one question per field, with *Autre…*, a note and previews,
like `user_ask`. A question without its `<k>_custom` field offers neither *Autre…* nor a note,
since nothing would carry them back. In a form of one question, the message's first line is the
question and the rest its text; a form that is one plain choice, such as the adapter's offer to
retry a refused request with another model, is sent only once you pick (*Refuser* keeps the
refusal). Nothing is chosen for you: a toggle starts off whatever the form suggests, a suggested
address is filled in only when it is a web address, a suggested date and time shows in your own
time. A number written with a separator followed by three digits (`1,000`), or with two
separators, is asked again, since it may mean a thousand or one. A `pattern` is not checked by Fronde: the asker checks its
own. The form's texts are cleaned as a permission card's are (hidden characters replaced, head
and tail kept), and the card says so. A form Fronde cannot show is declined at once, with the
reason in the answer's `_meta` (`fronde/refused`): another type (an object, say), another
`format`, more than 12 fields, a question with no option or more than 50, two options that read
the same, a number of picks other than one or any, a card over 64 KiB. So is a form that looks
like it asks for a secret, with a plain reason (see
[Security](security.md#forms-from-agents-and-servers)). A form for another session than Fronde's
is cancelled unseen. An agent's form still pending when Fronde restarts expires: the agent's call
that waited on it is gone.

The agent gets typed answers: each option's value (never its title), the text, the number (whole
as an integer), the toggle, and a question's free text and note joined under `<k>_custom`. A
field left blank is left out. *Refuser* gives the agent a `decline`, which Claude reads as a
skipped question. The card expires after **10 minutes**, and the agent then gets a `cancel`, as it
does when you press *Stop* or the turn ends. When the agent withdraws its form first (its tool
call was aborted), the card closes and a late answer goes nowhere. A form that is not a form
(ACP's URL mode, used for an MCP server's sign-in, which Fronde does not advertise) gets a
`cancel` without a card.

## Secret cards

`user_secret` asks for one secret on a card of its own, never in a question card. The card shows
why it is needed, a button to the sign-in or setup page when the agent gave one (https only, named
by its host), and a masked box that neither checks spelling nor fills itself in (browsers and
password managers are told it is a new password, not one to offer); *Afficher* shows what you
typed, *Masquer* hides it again. *Enregistrer* sends it, *Refuser* declines.

By default the secret is kept **for this conversation**: in Fronde's memory only, never in
`fronde.db`, for the agent that asked and in this conversation only (any later turn of it, not
only the one that asked), for one hour, and used up by its first use. When the agent proposes a
name and an origin, an admin also gets a box *Garder pour <origin>*: ticked, the secret is sealed
in `fronde.db` under that name, to be sent to that origin only, as `fronde secret set` would store
it. When a secret is already kept under that name for that origin, the card says so: *Remplace le
secret « <name> » déjà gardé pour <origin>*. Nobody else sees the box, and the server refuses a
kept secret from anyone but an admin. A name already kept for another origin is refused before
the card is shown, and the agent is told only that the name is taken, not for which origin.

The answer travels from the page to the server on the chat's WebSocket and goes straight to the
secret store. The page keeps nothing of it: the box is read only when you send, and emptied once
the answer went out; the secret is never shown again. If the connection drops before it went
out, the card stays open with *Connexion perdue, réessaie* and what you typed is still in the
box. On the server, the value becomes a `SecretString` as soon as the message is parsed, wiped
once it is stored. The message itself cannot be wiped: it may stay in the WebSocket library's
buffer until that memory is reused (see [Secrets in memory](security.md#secrets-in-memory)).
Then the request closes: its row keeps where the secret went (`{"kept": "conversation",
"secret_ref": …}` or `{"kept": "stored", "name": …, "origin": …}`), the conversation says
*Enregistré, jamais montré à l'agent* or *Gardé pour <origin>* (with *clé maîtresse créée :
trousseau*, *fichier* or *identifiant systemd* when that secret made the master key, to back up),
and the agent gets the reference or the name. Two pages answering at once keep one secret: the
first answer wins, and the second is refused before anything is kept.

A `secret_ref` is useless outside its conversation and to another asker, and every refusal says
the same thing, so a guess learns nothing. Outside clients that give the same name share one
asker (see [Security](security.md#secrets-given-in-a-card)). Starting a new conversation will
forget its references (**planned**). No tool takes one yet (see
[Tools](../reference/tools.md#user_secret)); the stored name is what a component can use today.

## Restarts and retries

The table is the source of truth. After a restart, a request whose time ran out expires, and so
do a pending permission, a pending form of the agent's and anything an upstream server asked: the
call that waited on each is gone, and its answer must never reach anyone else; one whose card never reached the conversation, because Fronde stopped
in between, is shown then; the others stay answerable. A tool call that is retried with the same `client_request_id` joins the
request it started instead of showing a second card. Secrets kept for a conversation do not
survive a restart: they live in memory only. A secret card still pending after a restart stays
answerable.

The id alone proves nothing: an agent reuses ids from one turn to the next, and outside
clients choose their names, so two of them can share one. A retry therefore joins a request only
when it asks the very same questions of the same user, and only while it is current: for a
thread's agent, in the same turn of the same thread; for an outside client, while the request is pending or within its
timeout. A cancelled request is never joined. Anything else is a new question: it gets a card of
its own, or an error asking for a new id while the request holding that id is still pending (the
table allows one pending request per caller and id). A new question never gets an old answer.

## Cards on a phone

Cards are built for a phone first: full width below 640 px, each option a row at least 44 px tall
that you can tap anywhere, *Envoyer* and *Refuser* full width. A question with one answer is a
radio group (arrow keys move the pick, as in any radio group); one with several answers is a list
of boxes. *Enter* sends the card, as in a form. *Autre…* reveals a text box, where the arrow keys
move the caret, and *Ajouter une précision* a note (not offered where the asker has nowhere to
put it). An option's preview shows under it on a phone and beside the options on a wider screen.
A text field is a 44 px box whose keyboard follows its format (e-mail, address, a date picker); a
number field is a text box with the phone's number keyboard, so that a decimal comma (`3,5`)
works; a date and time is sent with the browser's offset from UTC; a toggle is a box on a row
you can tap anywhere. An optional field says *(facultatif)*. Text from an agent is shown as text,
never as HTML.

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
- **Forms by URL**: an MCP server's sign-in page, sent as a URL elicitation, shown as a link on a
  card. Today such a form is cancelled.
- **Late answers**: an answer to a question whose asker is gone (after a restart, or a tool call
  cut short) reaches the assistant as a message. Today such an answer is saved but goes nowhere.
- **A tray** above the message box listing the open cards, and their count in the tab's title.

# Use several conversations

Fronde keeps several conversations, called threads, each with its own agent, history and files.
The chat you land on at `/` is the main thread; it is always there and is never archived.

## Open the list

On a phone, tap the list button (three lines) at the top left of the chat: the list,
*Conversations*, fills the screen. On a wider screen the same list sits to the left of the chat
as soon as there is more than one thread.

Each row shows the thread's title, how its last turn stands (*En cours*, *En file d'attente*
while it waits for a free agent, *Terminée*, *Échec*, *Interrompue*, or *Inactive* before any
turn), and its last activity: the time today, else the day. The most recently active threads
come first. A row marked *attend ta réponse* has a card waiting for you: open it to answer.

With a keyboard, Tab moves through the list and its buttons, and the up and down arrows move
from one thread to the next.

## Start a thread

Press *Nouvelle* in the list, or the plus button at the top right of a chat. Write the first
message and send it: that message creates the thread and becomes its title (its first 60
characters). If the connection drops before Fronde answers, the page asks again with the same
attempt, so a thread is never made twice.

## Switch between threads

Tap a row. Each thread keeps the message you were typing and where you had scrolled, so you can
come back to it.

## Find the cards waiting for you

While cards wait for an answer, the tab's title starts with their number, *(2) Fronde*, and a
tray above the message box says *2 demandes en attente*. It lists the open cards of the thread
you are in by their title: tap one to scroll to it. Below, *Ailleurs :* links to the other threads
that wait, with how many cards each. A card stays open after Fronde restarts, or after the agent's
tool call gave up waiting; your answer then reaches that thread's agent as a message from you
(see [Late answers](../explanation/requests.md#late-answers)).

## Rename or archive a thread

Each row and each chat's title have an actions button (three dots):

- *Renommer* turns the title into a text box. Type the new title and press Enter to save it, or
  Escape to cancel. A title has 1 to 80 characters on one line, without invisible characters.
- *Archiver* moves the thread to *Archivées*, folded at the bottom of the list. Its agent stops,
  the secrets you gave in its cards for this conversation are forgotten, and it can be read but
  not written to. The sub-agents it started stop their work too; they stay in the list with what
  they wrote. *Désarchiver* brings the thread back, not the work its sub-agents stopped.

When Fronde refuses an action, the reason shows above the chat, with a button to close it.

## See a sub-agent's work

When an agent hands a task to a sub-agent, the sub-agent's thread appears indented under its
parent's, marked *sous-agent*. Open it to follow the work. Its first message is the task, marked
*Écrit par* followed by the parent thread's title, since the parent wrote it, not you. A card the
sub-agent asks shows in its own thread, and its row says *attend ta réponse*. What you write in a
sub-agent's thread is read by its parent agent too, with `threads_read` (see
[Threads and sub-agents](../explanation/threads.md#how-the-answer-comes-back)).

When a sub-agent finishes, its parent's chat shows a small line, *Sous-agent terminé* followed by
the sub-agent's title (with *(interrompu)* or *(échec)* when it did not end normally). The title
is a link to the sub-agent's thread. The parent agent then answers on its own with what the
sub-agent found: you do not have to ask. If the parent was in the middle of a reply, it finishes
that reply first. Cards shown during such an answer say *Tour lancé par le résultat du
sous-agent* with its title: that turn follows the sub-agent's text, not yours, so its permission
cards offer no "always" option and it cannot ask you for a secret; nor can the sub-agents it
starts or sends to. Stop interrupts that answer only; the sub-agents keep working. Results that
were waiting when you pressed Stop, and those that come after, then wait for your next message,
which carries them, and its cards say *Ce tour transmet N résultats de sous-agents*. After ten such answers in a row without a
message from you, the next results wait the same way, and the chat says how many. An archived
thread is not woken, nor are the threads under it. Archiving a sub-agent itself shows a
*Sous-agent archivé* line in its parent's chat.

Stop in a sub-agent's own thread stops that sub-agent; its parent is told it ended
*(interrompu)*. A sub-agent's turn that runs longer than 30 minutes is stopped the same way, with
*Délai dépassé* in its thread.

## When the subscription's limit is reached

When your subscription's 5-hour or weekly window is used up, Claude refuses the turn, and the
thread says when it can run again, in the server's time:

> Limite d'usage atteinte, reprise vers 16:40.

The turn ends as failed. Send your message again after that time. Fronde keeps the reading the
refusal carried (see [Usage and quotas](../explanation/subscriptions.md#usage-and-quotas)).

## Change the limits on sub-agents

Sub-agents spend your subscription's quota in the background. When an agent says it hit a limit,
the message names the setting, in `[threads]` of `fronde.toml`:

```toml
[threads]
max_depth = 1             # 0: no sub-agents; 2: a sub-agent may start its own
children_running = 3      # per thread, at once
children_per_day = 30     # all threads together, in 24 hours
children_kept = 200       # per thread, not archived
child_turns_per_day = 100 # each task and each threads_send, all threads, in 24 hours
child_turn_timeout = "30m" # also caps an answer a sub-agent's result started
wakes_without_owner = 10  # answers to sub-agents' results in a row without your message
```

Restart Fronde for a change to apply. Raise a limit only for work you trust: together they bound
what an agent misled by something it read could spend without you (see
[Threads and sub-agents](../explanation/threads.md#limits)).

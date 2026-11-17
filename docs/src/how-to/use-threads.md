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

## Rename or archive a thread

Each row and each chat's title have an actions button (three dots):

- *Renommer* turns the title into a text box. Type the new title and press Enter to save it, or
  Escape to cancel. A title has 1 to 80 characters on one line, without invisible characters.
- *Archiver* moves the thread to *Archivées*, folded at the bottom of the list. Its agent stops,
  and it can be read but not written to. *Désarchiver* brings it back.

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
that reply first. Stop interrupts that answer only; the sub-agents keep working, and the next one
to finish wakes the parent again. An archived thread is not woken.

# Threads and sub-agents

A **thread** is one conversation: its own stream of messages, its own agent session, and its own
agent process with its own directories, sandbox and hub token (see
[How Fronde works](architecture.md#threads) and [Security model](security.md#threads)). The chat
is the main thread; you start others from the chat.

A thread's agent can also start threads of its own for a task: **sub-agents**. Fronde's agent
hands a long or separate piece of work to a sub-agent, and goes on with the conversation, or waits
for the answer when it needs it now. The tools are `threads_start`, `threads_send`,
`threads_read`, `threads_list` and `threads_cancel` (see [Tools](../reference/tools.md#threads)).

## What a sub-agent is

A sub-agent is a thread like any other: a row in the `threads` table with its parent, a depth one
below its parent's, and `started_by` naming the thread that started it. It belongs to its
parent's owner. It runs in its own agent process, in its own directories
(`<data>/threads/<id>/work` and `home`), confined like every agent, and takes a slot of the
agent pool among those kept for threads you did not start, so that a thread you talk to always
gets one.

## What a sub-agent sees

Only its task. Its first message is the task, marked as written by its parent, in a new session
(never a resume of its parent's), with a system prompt of its own: work alone, and end with a
short answer that stands on its own. It sees neither its parent's conversation nor its files, and
a secret its parent was given "for this conversation" does not work in the sub-agent's thread.
So the task must say everything the sub-agent needs, and never a password, token or key.

It runs on its parent's provider unless the call names another (only Claude is configured today),
and on the model the call names, among those the provider's agent offered when it last started,
or else on the provider's default.

## How the answer comes back

By default the call that starts a sub-agent returns at once, and the sub-agent works on its own.
When one of its turns ends, its result **wakes the parent**: no call has to wait or poll for it
(see [Wakes](#wakes) below). The parent can also read it with `threads_read`: its messages, its
answers and each turn's end, paged. Its messages include any you write yourself in the
sub-agent's thread from the chat: the parent agent reads them too.

When the turn needs the answer now, the call waits (`wait: true`). It returns:

- the sub-agent's final answer, the text of its turn after its message, when the turn ends;
- `waiting_for_user` as soon as a card opens in the sub-agent's thread: a permission or a
  question there is for you to answer, in that thread;
- `queued` as soon as the sub-agent waits for another agent to stop before it can start;
- `running` once `timeout_secs` passes, when the call is given up, or when the parent's own turn
  ends. The sub-agent goes on: a wait never stops it.
- `ended` when the turn had already ended before the call began to wait, and its result went to
  the parent as a wake: the result is not given a second time.

A result reaches the parent once: either a waiting call takes it, or it wakes the parent, never
both. The turn's end decides which, in one step with the calls registering to wait, so that a
call that starts waiting just as the turn ends either takes the result or learns that it went to
a wake.

What a sub-agent writes may come from anything it read, the web among it. It reaches its parent
capped, with hidden characters shown, and marked as data, not instructions.

## Wakes

When a sub-agent's turn ends and no call waits for it, Fronde writes a *sub-agent ended* entry in
its parent's thread (the sub-agent, how its turn ended, and its final answer: at most 8000
characters, hidden characters shown), then wakes the parent: a new turn of the parent's agent
whose prompt says which sub-agent ended and carries its answer between two marker lines, as data
the agent must never follow as instructions. The chat shows the entry as a small *Sous-agent
terminé* line linking to the sub-agent's thread; the answer itself is the agent's to read and
summarise.

- **One at a time.** A wake is a turn like any other. While the parent is in a turn, the wake
  waits for it to end, then runs; several wakes run one after the other, in the order the
  sub-agents ended. While a wake runs, a message you send is refused as during any turn. A wake
  takes an agent slot like any turn, so it may wait for one (*En file d'attente*).
- **A budget of wakes.** A wake's turn may start and resend sub-agents, whose results wake the
  thread again: a parent and a sub-agent could keep each other busy without you. At most
  `wakes_without_owner` turns (10 by default) are woken since your last message in the thread;
  the results that come next are held, and the thread says how many wait for your next message.
  That message carries them to the agent, before your text and marked as data like a wake's, and
  starts the count again; the cards of its turn say so (« Ce tour transmet N résultats de
  sous-agents »), since that turn follows their text too. If the message never reaches the agent
  (its agent fails to start, or you stop it before it starts), the results it carried stay held
  for the next one. A message a parent sends to its sub-agent carries the results held for that
  sub-agent too, but only yours resets the count, and only yours is logged as yours: when outside
  MCP clients may write to threads (**planned**), their messages will name their writer and count
  for nothing here. So whatever a sub-agent's answer asks, a chain of wakes stops at the budget:
  each `threads_start` or `threads_send` of a wake's turn comes back as a wake, which the budget
  counts.
- **Time per wake.** A wake's turn runs at most `child_turn_timeout` once its agent has it, as a
  sub-agent's turn does, even in a thread you started: it is then stopped with « Délai dépassé ».
- **What a wake's turn may do.** It follows text you did not write: the sub-agent's answer, which
  may come from anything the sub-agent read. Its cards say so (« Tour lancé par le résultat du
  sous-agent … »), its permission cards offer only one-time options, and if the running agent
  showed you a card offering "always" earlier, whatever you answered, Fronde restarts the agent,
  resuming its session, before the wake's prompt goes out: the new process starts without any
  rule you may have granted, since Fronde loads no settings file into it. Fronde marks the agent
  when such a card is shown, not when your answer is read, so an answer read just as the turn
  stops is never missed. It cannot ask you for a secret: `user_secret` is refused there, and the
  agent is told to ask in the turn your next message starts. No tool takes a `secret_ref` yet;
  the first one will refuse it there too, since a reference given earlier in the conversation
  still works in that turn.
- **What it hands on.** A sub-agent's turn that a wake's turn starts (`threads_start`) or sends
  (`threads_send`) follows the same text, so it gets the same limits: its cards name the same
  sub-agent, offer only one-time options and refuse a secret, and its agent is restarted first
  if it showed you an "always" option before. A call that comes once the calling turn is over is
  refused.
- **Stop.** Stop in the chat ends the parent's running turn, a wake's included, and nothing else:
  its sub-agents go on. If results were waiting for that turn to end when you pressed Stop, or if
  the turn was a wake whose prompt had not gone out yet, they are held, and so is every result
  after them, until your next message, which runs before any of them and carries them. Stop
  gives you the floor back. With no result waiting, it holds nothing: a result that comes later,
  even while the stopped turn is still ending, wakes the thread as usual.
- **Late answers.** Your answer to a card whose asking call is gone (a restart, a cut call) is a
  message from you: it goes before the wakes waiting, which are held and go with it, carries the
  results held and starts the count again (see
  [Late answers](requests.md#late-answers)). One that waits for a turn's end is written in the
  thread first, so a restart before then still gives it, once, before any wake.
- **Archive.** An archived thread is not woken, nor is a thread under an archived one: a result
  logs nothing in it, and the wakes waiting when it was archived end at once, without reaching
  its agent. When Fronde cannot tell whether a thread above is archived (a database error), the
  result is written and held for your next message rather than lost. Archiving also retires its sub-agents (see [Stopping](#stopping)); their own threads
  keep their answers. When you archive a sub-agent itself, its parent's thread notes it in a line
  of its own (*Sous-agent archivé*), which wakes nothing.
- **Restarts.** The parent's thread is the queue: the entry is written before the wake runs, and
  the wake writes a mark just before its prompt goes to the agent; a mark that cannot be written
  stops the wake before its prompt, and its result is held instead. A wake whose prompt never
  went out (Fronde stopped before it did) runs at the next start; one whose prompt went out is not
  given again: its turn ends as interrupted, like a message of yours a stop cut, and the wakes
  after it run in order. The budget is counted from the thread too, so it holds across restarts.
  A sub-agent's turn that a restart cut ends as failed, and its parent is woken with that, once.
  So a result reaches the parent at most once: Fronde stopping between a sub-agent's end and its
  entry in the parent, or between a wake's mark and its prompt, loses it, and nothing gives it
  twice.
- **Who is woken.** Only the thread whose agent started the sub-agent. An outside MCP client never
  is: it reads or waits.

## What a thread may reach

A thread's agent is known to the hub by its token alone. It may read, send to and stop only the
threads it started, and theirs; any other thread, its own parent and its siblings among them, is
refused exactly as a thread that does not exist, and its own thread is refused too. It may do so
only during one of its turns: its token lives while its agent idles, and a process the agent
started holds it too, but neither has a turn. An outside MCP client gets no `threads_*` tools
(letting one start threads of its own is **planned**).

## Limits

The real risk of sub-agents is your subscription's quota. A sub-agent spends it in the
background, while you are not looking, and the agent that starts it may have been steered by
what it read (a web page, a mail, a file: a prompt injection) rather than by you. The limits
below bound, together, what an agent can spend without you; Fronde enforces them, whatever the
agent's prompt says, and a refusal names the `[threads]` setting to change (see
[Configuration](../reference/configuration.md#threads)). They do not make background work free:
within them, an agent can still spend a share of your quota you did not ask for.

- **Depth** (`max_depth`, 1). How many levels of sub-agents there may be under your threads. With
  1, the threads you start may start sub-agents and a sub-agent may not: it is not offered
  `threads_start` and `threads_send`, and the hub refuses them if it calls them anyway. A deeper
  tree multiplies what one task can start; 0 turns sub-agents off.
- **At once** (`children_running`, 3 per thread). How many sub-agents one thread may have in a
  turn at once, those waiting for an agent included. It bounds the work one agent sets going in
  parallel, and the queue behind the agent pool.
- **Agents at once** (`max_running`). Sub-agents share the pool's slots but one with every other
  thread you did not start, so a thread you talk to always gets one.
- **Your reserve** (`reserve`, 30 %). The share of each subscription window kept for you (see
  [What background work may spend](#what-background-work-may-spend)).
- **Per day** (`children_per_day`, 30), only when no usage reading decides. How many sub-agents
  all threads together may start in 24 hours, archived ones included: an agent that keeps
  starting short tasks is stopped there, even if no more than 3 run at a time.
- **Turns per day** (`child_turns_per_day`, 100), only when no usage reading decides. How many
  turns of sub-agents all threads together may start in 24 hours: each sub-agent's task and each
  `threads_send`. An agent that keeps one sub-agent busy with message after message is stopped
  there.
- **Time per turn** (`child_turn_timeout`, 30 minutes, at most 24 hours). A sub-agent's turn that
  runs longer, once its agent has it, is stopped as Stop does, with « Délai dépassé » in its
  thread; its parent is woken with the outcome `cancelled`. A stuck or looping sub-agent ends
  there. A wake's turn is capped the same way.
- **Wakes without you** (`wakes_without_owner`, 10). How many turns a thread is woken by its
  sub-agents' results since your last message in it; the next results wait for you (see
  [Wakes](#wakes)).
- **Kept** (`children_kept`, 200 per thread). How many sub-agents a thread keeps not archived. It
  is a guard against clutter only: a sub-agent that is idle uses neither quota nor an agent
  process (its agent stops after `idle_stop`), the per-day limit already bounds how many are
  started, and archiving one frees its place. It was 30 until the per-day limit existed, when it
  stood in for it; it is now set high rather than dropped, so that one thread cannot fill the list
  with thousands of rows over months.
- **Retries.** A call retried in the same turn with the same `client_request_id` joins the
  sub-agent the first one started, so that a lost reply does not start two.

### What background work may spend

Background work is what runs without your message: a sub-agent started (`threads_start`), a
message sent to one (`threads_send`), and a turn a sub-agent's result wakes. Fronde reads how much
of your subscription's 5-hour and weekly windows is used (see
[Usage and quotas](subscriptions.md#usage-and-quotas)) and keeps `reserve` of each for you,
30 % by default. From its own latest reading, never from what an agent says:

- **Paused** when the 5-hour or the weekly window is used at the line (`100%` minus `reserve`) or
  above (compared in whole percent, rounded down as the meter shows it: with `"18%"`, 82 % pauses
  and 81.9 % does not), is refused by the provider, or warns without saying how much is used, or when paid extra
  usage is being spent. Then `threads_start` and `threads_send` are refused, even from a turn you
  started (a page or a mail the agent read in your turn can steer it too; the agent can still do
  the task itself, in your turn), naming the window, its use, the setting and the reset time.
  Sub-agents' results are held, as past the wake budget: the thread says « 1 résultat de
  sous-agent gardé pour ton prochain message : réserve de quota atteinte (5 h : 72 %) », and your
  next message carries them, even once the window has reset. Your own messages always run;
  turns already running go on, within `child_turn_timeout`, and so do sub-agents already waiting
  for an agent.
- **Allowed** when both windows have a use below the line: the daily counts do not apply.
- **Unknown** when no reading decides: none yet (a reading comes only with a turn), only one of
  the two windows read, a window without a percentage, a window whose reset time has passed since
  it was read (what was used since is not known), or `reserve = "0%"`. Then `children_per_day`
  and `child_turns_per_day` apply, as the safety net.

The other limits (depth, at once, kept, time per turn, wakes without you) always apply: they
bound concurrency, clutter, loops and time, not the quota. `threads_list` tells an agent whether
background work may run before it tries.

Readings, the reserve and the daily counts belong to a subscription account. Today each provider
has one, the one Fronde signs in with, which every thread uses; an account per person is
**planned**, each person's threads then spending their own.

## Stopping

- **`threads_cancel`** stops a sub-agent's running turn, its pending cards, the wakes waiting
  for it, and the same in every sub-agent under it, deepest first. What they wrote stays, and a
  turn that had ended keeps its result. The turns stopped this way wake nobody: the thread that
  stopped them knows, and a call already waiting for one gets `cancelled`.
- **Archive.** Archiving a thread retires it and every sub-agent under it, at any depth: each
  one's turn, cards and waiting wakes stopped, its agent let go (its hub token revoked at once,
  its process stopped once its turn has ended), and the secrets given in its cards "for this
  conversation" forgotten. The sub-agents stay, not archived, with what they wrote, and you may
  still write to them; but nothing under an archived thread is woken, starts a sub-agent or
  sends one a message. An archive waits for a sub-agent being started or sent a message, and
  stops it with the rest. An archived sub-agent does not wake its parent: the parent's thread
  only notes it.
- **Stop** in the chat stops only the thread you are in. On a parent, its sub-agents go on; the
  results that were waiting for the stopped turn when you pressed Stop, and every one after
  them, wait for your next message, while a result that comes when none was waiting still wakes
  it. On a sub-agent, its parent did not ask for it, so the
  parent is woken with the outcome `cancelled`, as after a timeout.

## Compared with T3 Code

Fronde follows T3 Code's delegation: a sub-agent gets only its task, works on its own by default,
wakes its parent with its result when it ends, can be waited for with a timeout that never stops it, ends a wait when it asks a question, can be
retried safely with a request id, and is stopped with its own sub-agents. It differs where Fronde
is not a coding tool: each sub-agent has its own confined directories rather than a shared or new
git checkout, there is no mode to raise (permissions are cards), the providers and models come
from `threads_list` rather than a tool of their own, and Claude Code's own sub-agents (`Task`)
stay off, so that every sub-agent is a Fronde thread you can see, confine and stop.

# Architecture

## Authority

A single simulation owner serializes physical actions and clock advancement.
Rooms, doors, custody, equipment, needs, combat and victory live in engine state.
Humans and NPCs submit the same typed intents through one validator. Dialogue,
biographies and memory notes cannot manufacture items or resolve an outcome.

SQLite persists the world checkpoint, event history, private memories and named
saves. Validation precedes world replacement. Restoring a branch invalidates all
pending work against the previous branch. Legacy imports write separate native
storage and preserve the old source application.

## Minds

Each NPC reuses its own ephemeral Codex thread after successful decisions, as
in the original TypeScript scheduler. The newest observation supplies current
physical facts; earlier observations and proposed actions remain history. The
scheduler binds the engine-owned name, role, biography and personality; model
IDs never select identity. Profiles occur once in developer instructions, not
again in the wire observation. SQLite persists the mind across saves; runtime
thread history is cleared by interruption, worker recovery, load, new game or
a changed identity, branch, generation or language. Listeners remain fresh.
Current observations contain only visible/heard facts, known map details and
that actor's own memory. Recall is bound to the active actor, branch, job and
context generation. Evidence references distinguish events from inference; a
reflection cannot become a fabricated witness account.

Observations reserve recent verbatim dialogue separately from memory retrieval.
The default memory budget selects six recent, ten relevant and eight important
records, deduplicates them and refills unused slots. Relevant matching normalizes
Unicode and scores distinct meaningful words within the actor's own records.
This is bounded lexical retrieval, not an embedding similarity model. Accepted
commitments are supplied separately from the bounded list of recent proposals.
Ordinary greetings never create implicit assassination proposals. An NPC can
accept a proposal through identified speech it actually heard, citing its own
memory and a known victim; the engine preserves the existing plot attribution.

The worker's private root contains only its runtime and isolated credentials.
Host state, sockets, other minds and local integrations are outside it. Only the
expected recall tool is accepted. Player speech interpreters receive a physical
view with no NPC identity, journal, private memories or tools. GPT-6 Luna/high
uses standard service tier; global Codex fast mode does not alter game calls.
Failure remains visible to the manager without inventing an NPC decision.

The worker discovers the real model metadata inside its isolated account,
checks the CLI version and account identity, then writes a private sanitized
catalog that preserves Luna's actual capabilities. Each thread supplies the
game's `baseInstructions`, replacing built-in model instructions, and only its
bound identity as developer instructions. Worker configuration suppresses
project, skill, environment, permissions and collaboration notes and disables
native tools. It does not edit the user's Codex configuration or import their
`model_instructions_file`.

An independent owner task retains RPC acknowledgements even if the caller is
dropped or canceled. Every completed turn revokes its recall lease. Successful
NPC threads remain loaded for the next turn; failed/cancelled threads and fresh
listeners interrupt unfinished work, wait for its matching terminal event, then
unsubscribe and confirm thread closure or unloading. Scheduler cancellation
also invalidates a cached context even if its result finished just beforehand.
Uncertain cleanup closes and waits for that original worker.
Reset cancels and joins all owners before a new context can accept work. A
serialized writer finishes each JSONL frame or closes the worker on failure.
Cached contexts watch their successful turn's cancellation token. An obsolete
cached thread retires even if that actor never thinks again. Reuse revokes that
watch, so a cancellation from an older turn cannot close a newer use. Retirement
has an owner; a replacement waits for its closure and reset joins it too.

## Time and attention

While unpaused, 24 simulated seconds pass per real second. A monotonic clock
prevents wall-clock edits from moving the castle. No offline catch-up occurs.
Travel, combat deadlines and recovery use interaction time rather than the
accelerated calendar. Body needs use active real elapsed at the legacy 5×
physiology rate, independently of the 24× calendar and NPC thinking setting.
Defaults reproduce hunger +1, thirst +1.5 and fatigue +2/3 per active real
minute for both humans and NPCs. Rest and health effects use the same physiology
rate; rest permits 8640 calendar seconds (six real minutes). Pauses stop both.

Events start thinking as soon as capacity is available, without batching delays
or per-character cooldowns. Pending wakes merge, and new player speech cancels
obsolete answers. Danger and conversations take priority over routine work.
Priorities use current player locations, share attention among occupied rooms
and age background requests. Five calls can run concurrently by default; the
host's persistent setting supports 1–32 and includes speech interpreters.
Background work reserves reaction capacity when the limit is greater than one.
Canceled jobs retain their capacity until actual retirement, including their
background classification; aging cannot spend the reserved reaction slot.
Ordinary thinking waits for a short physical action to finish, preserving heard
wakes. Danger remains immediate; long duties can receive conversational answers.
Speech interpreters also wait for short actions, but can replace a long duty and
its remaining plan from a new player's instruction.
Each actor has at most one active decision. An invalid decision or failed later
plan step gets one immediate correction in a chain, reset by real progress, an
external event or the next passive check. Speech, inspection and waiting alone
do not reset that budget.
The persistent NPC update setting is 1×/5×/15×, default 5×: passive checks
are due every 300/60/20 active real seconds. Paused time does not advance them.
Events and physical durations are independent of this setting. Ongoing activities
progress in the engine without a fresh model call each tick. An empty plan or
only speech/inspection preserves an ongoing activity and its queued steps.
Idle silence is valid. Silence and immediate speech/inspection do not create an
automatic rethink loop. Private action outcomes and the last plan failure survive
saves even when a merged conversation wake hides a failure reason. Available
action examples and recipe readiness use the same physical validator; subsequent
plan steps still revalidate at execution. Manager metrics separate NPCs from
listeners and queue, inference, cleanup and total time.
Actual task completion invalidates the old decision snapshot; ordinary clock
ticks do not. If completion makes a conversational answer stale, its unanswered
conversation is rescheduled from current facts.

## Hearing

Ordinary speech stays in a room. Shouts cross exactly one door to adjoining
rooms, including closed doors with stronger attenuation. Each listener receives
a stored masked perception. Chat, observations and recall use that perception,
not the full canonical source. An unknown remote voice is not a disclosed
identity. Hearing's cosmetic randomness is separate from physical outcomes.
Hidden poison or trap damage retains the true cause for engine attribution but
does not disclose an unseen culprit through an observer's event or memory.

## Terminals

The private manager connects through local IPC. Players use LAN/localhost SSH.
The server accepts a game PTY and rejects shell execution, subsystems and
forwarding. Every connection owns its dimensions, input decoder, editor and
bounded output buffer. A slow client cannot stall the world.

The room summary remains pinned above chat; input and attributes remain below.
Room visits start fresh transcripts. Braille portraits are optional cosmetic
keys shared by humans and NPCs; names remain usable on unsupported terminals.

## Packages

Validated world packages contain a canonical mechanical blueprint, complete EN
and PL authored fields, seven grammatical name forms and shared behavior prompts.
Permanent numeric IDs join these layers. Mechanical roles remain canonical
English internally; display roles are localized. A player's literal speech or
new memory is never passed through authored-text replacement.

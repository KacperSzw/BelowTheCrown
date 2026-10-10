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

Each character has a scheduler-owned Codex thread. Model-provided actor IDs never
select identity. Current observations contain only visible/heard facts, known
map details and that actor's own memory. Recall is bound to the same actor.
Evidence references distinguish events from inference; a reflection cannot
become a fabricated witness account.

The worker's private root contains only its runtime and isolated credentials.
Host state, sockets, other minds and local integrations are outside it. Only the
expected recall tool is accepted. GPT-6 Luna/high uses standard service tier;
global Codex fast mode does not alter game calls. Failure remains visible to the
manager without inventing an NPC decision.

## Time and attention

While unpaused, 24 simulated seconds pass per real second. A monotonic clock
prevents wall-clock edits from moving the castle. No offline catch-up occurs.
Travel, combat deadlines and recovery use interaction time rather than the
accelerated calendar.

Events start thinking as soon as capacity is available, without batching delays
or per-character cooldowns. Pending wakes merge, and new player speech cancels
obsolete answers. Danger and conversations take priority over routine work.
Priorities use current player locations, share attention among occupied rooms
and age background requests. Four calls can run concurrently; background work
cannot consume all reaction capacity. Each character has at most one active
decision, and a rejected decision gets one immediate correction attempt.
Idle characters are reconsidered every 30 real seconds. Ongoing activities
progress in the engine without a fresh model call each tick.

## Hearing

Ordinary speech stays in a room. Shouts cross exactly one door to adjoining
rooms, including closed doors with stronger attenuation. Each listener receives
a stored masked perception. Chat, observations and recall use that perception,
not the full canonical source. An unknown remote voice is not a disclosed
identity. Hearing's cosmetic randomness is separate from physical outcomes.

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

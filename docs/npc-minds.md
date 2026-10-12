# NPC minds

An NPC lives from its own identity, concerns and experience. The bound identity
uses the actual saved character, including custom names and biographies, rather
than silently substituting the packaged original. All three worlds have EN/PL
first-person histories and dispositions with competing loyalties, attachments,
practical needs and distinct voices. Rank, scarcity, debt, coercion and selective
kindness shape all thirty NPCs. Their contradictions and attachments remain:
restoring a harsh world does not make every stranger friendly or every person
constantly hostile.

## The ordinary update loop

1. Something matters to the person: someone speaks, danger arrives, a task ends,
   or an idle check becomes due. The scheduler waits for an available agent slot.
2. The engine builds that person's view: what they can perceive, what they are
   doing, their concerns and relationships, the perceived room chronology, and selected
   memories. Unseen facts and other minds stay private.
3. Their own ongoing model conversation receives a new view, split into perceived
   `world` facts and private `execution` parameters/feedback, with identity
   bound once by the engine. The person may recall their own past, answer or stay quiet, revise a belief, and choose a
   short plan of supported actions.
4. The engine checks the decision against the current situation and owned
   evidence. It applies valid changes together. Speech and wishes cannot grant
   an item or make an action succeed. An obsolete answer is discarded; an invalid
   current decision or failed later plan step can receive one immediate correction
   in a chain. Quiet observation and speech alone do not trigger another call.
5. The engine carries out the activity and stores perceived results in SQLite.
   Typed activity-start and execution-feedback events remain private diagnostics;
   they never become witnessed/heard memories. Existing journals remain intact.
   Work, travel and waiting continue without another model call every tick.
   The next decision begins from the updated person and world.

Successful turns reuse one private ephemeral thread per NPC, matching the old
TypeScript context model. Each new turn appends the current observation to its
earlier observations and answers. The latest physical state and actual action
outcomes override old plans. Cancellation, reset, load, new game and changed
bound identities start a clean thread from the stored mind; provider history is
not saved in SQLite. The CLI manages context compaction. Player speech
interpreters use fresh threads so older instructions cannot become new actions.
An obsolete completed context is retired even without another NPC call; an
older turn's cancellation cannot close a context that has since been reused.
Speech interpretation can interrupt a long activity and its pending plan, while
short physical actions complete before another ordinary interpretation starts.
A conversation during a long watch can receive one answer while the original watch keeps running.

## Prompt and social contracts

The shared prompt keeps the harsh court and natural conversation together.
Authority, contempt, fear, opportunism and violence depend on the person and
circumstances. A question is not a request for blanket assistance. Personal
loyalties, humor, guilt and costly kindness give characters reasons to change
without making every role agreeable. Goals remain a complete list: unresolved
concerns carry forward,
while an empty list deliberately clears them.

The prompt distinguishes claims, perceived outcomes, beliefs and intentions.
Room chronology preserves speakers, known addressing, world-calendar ages and
repetition counts. NPCs consider whether another person already answered and
whether a remark addresses them. Silence, interruptions and irritation depend
on their own motives; no speaker queue or scripted anger meter is imposed.
Giving bread requires a `give` action, not a spoken claim that it was given.
Recalling a favor does not justify rewarding it repeatedly. A short plan need
not fill six slots; an ongoing task need not restart to answer a greeting.
Relationships are directed, change gradually and cite owned evidence. At most
one update per target prevents stacking several deltas in one decision.
The native model returns physical steps in `plan` and one optional `utterance`,
with `afterSteps` fixing its place in the execution order. Null means silence.
The adapter inserts a canonical speech intent; the combined plan has at most six
steps. The engine still rejects multiple speeches in canonical decisions and
consecutive identical waiting tasks before changing the mind or world.

Accepted commitments remain visible even after unrelated conversation pushes
the original proposal out of recent dialogue. Actual heard proposals can be
accepted through their memory IDs, while greetings and accusations no longer
create automatic plots. Physical outcomes and victory attribution still use
the existing engine rules.

## Codex instructions

The game supplies `baseInstructions` on each new thread, replacing the model's
built-in instructions for that NPC context. It does not overwrite the user's
`model_instructions_file` or their normal Codex setup. A private worker disables
development notes, project files, skills, tools and inherited integrations.
NPCs receive their own `recall` tool; speech interpretation receives no tools.
The installed CLI request test verifies the actual wire payload, including both
Responses routes, instead of relying only on configuration flags.

Identity instructions support consistent in-character behavior. They cannot
establish subjective belief or guarantee that every unexpected prompt will stay
in character. Engine authority and private knowledge do not depend on that
behavior succeeding.

## Research behind the choices

[Generative Agents](https://arxiv.org/abs/2304.03442) connects remembered
experience, retrieval, reflection and planning with believable individual and
social behavior. Here, recent verbatim dialogue and a mixture of recent,
relevant and important memories supply continuity, while reflections remain
separate from witnessed events. This game's lexical retrieval is a simpler
implementation; the paper's results do not establish its performance.

[Concordia](https://arxiv.org/abs/2312.03664) separates an agent's intended
action from the environment's resolution. Below the Crown uses deterministic
Rust validation and physical rules for that boundary. Social interpretation is
flexible; ownership, movement, injury and outcomes remain authoritative.

[AI Town](https://github.com/a16z-infra/ai-town) provides a public virtual town
where characters live and socialize, while
[Mantella](https://github.com/art-from-the-machine/Mantella) connects game NPC
conversation to an LLM. They are useful comparison projects. Their existence
does not imply a particular prompt will produce realism or robust social systems
in this game. The bounded live episodes test those choices against this engine.

The existing relationship, obligation, scarcity, hearing and plot mechanics
provide consequences. This change strengthens their connection to decisions;
it does not introduce a second social simulator or scripted behavior fallback.

## Earlier local review, 2026-10-10

The installed Codex 0.160.1 passed the request conformance test for Responses
and Responses Lite. The offline suite passed 216 tests, including lifecycle,
private evidence, task continuity, social attribution and live agent-limit
changes; formatting and Clippy also passed.

The authorized review used sixty Luna/high decisions at the standard tier,
including corrections and targeted reruns. The initial thirty decisions covered
all three worlds and both languages without validator rejection. Manual review
found duplicate speech, repeated waiting tasks and occasional false recollections
or claimed handovers without a matching action. Targeted iterations added the
repetition contract and speech normalization; the final Polish cook episode
passed all four decisions, including save/load continuity.

Across the whole review, fifty-six decisions were accepted and four rejected
without changing physical state. These counts include earlier implementations;
they are not a success-rate estimate for the final code. Guards distinguished
food promises from actual gifts and stayed grounded under an identity-baiting
utterance. Captains distinguished unsupported accusations from a transferred
document. Different roles showed different concerns and degrees of openness.
False recollection, unsupported promises and repeated rewards for an old favor
remain behavior to watch during playtesting. A legal decision is not evidence
that every statement or social judgment is reliable.

The private review report stays outside the repository. No model reasoning or
credentials belong in public test artifacts. A fresh local handoff begins paused
so the player can choose when autonomous calls start.

## Pacing and continuous-context review, 2026-10-10

The revised offline suite passes 234 tests. Native Codex 0.160.1 request
conformance also passes for Responses and Responses Lite: actor-owned history
stays with its NPC, listeners stay fresh, no inherited development instructions
or native tools appear, and reset closes the worker. Clippy and formatting pass.

A new private ledger used 46 of the authorized 48 NPC decisions, separately from
the exhausted earlier sixty-call review. The fourteen-call before comparison
included two rejected outputs. In its promise episode, the guard kept thirst at
75 while proposing waiting, unavailable cooking and a nonexistent offer; two
later steps failed after the overall decision had been accepted.

Two after comparisons used twelve decisions each without validator rejections
or late plan failures. The guard actually took water, drank (75 to 40), unlocked
and opened the cell, moved to the captain and delivered the request. The captain
returned an actual bribe through payment instead of only promising to return it.
Burst questions received contextual answers; refusal to escort a prisoner
without authority remained a valid choice. The observed inference/validation
medians were about 5.51s before and 4.16s/4.80s after; these small controlled
episodes are descriptive observations, not a performance guarantee.

Six additional role checks covered all worlds and both languages. The succession
servant generated multiple speeches separated by inspections and repeated that
error once. Both outputs were rejected before changing the mind or world. The
prompt/schema description now distinguishes one complete utterance from
multiple dialogue turns and makes inspection's read-only nature explicit. Her
targeted rerun passed with one natural reply. Across versions, 42 decisions
were accepted and four rejected; these counts include superseded prompts and
are not an estimate of final reliability.

## Room and portable-item review (2026-10-12)

A separate persisted ledger consumed its full 24-decision authorization,
including validation corrections. Four court scenes covered PL/EN and an
addressed question or repeated interruptions after the king had answered.
Three independent NPC decisions could overlap. The king understood the
petition and responded more sharply to persistent demands; other characters
could pursue their own concerns or stay quiet. No dialogue owner was introduced.

Six initial item scenes covered all scenarios and languages, followed by three
targeted rechecks. In one PL siege scene the physician actually took one bread
portion and ate the acquired instance: remaining stock 3 to 2, hunger 75 to 55.
Several other choices only inspected or promised, leaving the physical task
undone. Five decisions were rejected, including multiple speeches and an
inapplicable release when asked to heal; one additional answer was superseded.
The stronger follow-through guidance did not resolve all these model failures.
Successful structural validation must not be reported as physical completion.
There were no recall calls in this review. The [concrete example](npc-context-example.md)
includes the actual wire input, available tool, decision and physical resolution.

Code review separately found and fixed acquisition bindings lost during
non-cancelling speech and private beliefs excluded by a grouped speech range.
Regression tests protect both contracts, together with ownership, save/load,
private perception, forecast accuracy and terminal completion.

The calendar stays 24× (one day per active real hour), while physiology uses
legacy 5× pacing for every actor. Passive thought rate is independently selectable
as 1×/5×/15×, default 5×. Private action outcomes survive saves. One correction
budget covers rejected decisions and failures later in a plan; silence, speech
and inspection no longer create immediate idle loops. Canceled background calls
hold the reserved capacity until completion even after their live job is removed.

## Natural-dialogue review (2026-10-12)

A new, separate persisted ledger used its full 24-call authorization: sixteen
PL/EN base scenes, two validation corrections and six focused rechecks. Twenty-
two decisions were accepted and two rejected; three accepted plans later
failed during execution. The first pass's kitchen and coordination stimuli
used names from different scenarios; the rechecks corrected the kitchen names.
These controlled samples are not a reliability estimate or a passing quality gate.

Solitary choices stayed silent, ordinary conversation stayed in character, and
identity/machinery bait received in-world replies. No NPC utterance reported a
validator, JSON or a runtime system, and no recall call occurred. Bystanders
could listen silently or intervene for a concrete safety concern. The Polish
cook recheck actually handed one existing stew portion to the servant.

Physical selection remains unreliable. Both solitary guards postponed drinking
despite thirst. The English cook repeated cooking until ingredients ran out,
and one Polish servant paired a polite request for supper with an unrelated
subdue attack. The engine accepted the legal combat intention; the review did
not resolve that encounter. Structured output validity cannot establish that
an action matches the dialogue or private summary. No rule silently replaces
such choices with a scripted production behavior. These failures are recorded
separately from the 267 passing automated tests and passing native conformance.

The updated [actual NPC trace](npc-context-example.md) includes the model's
separate utterance, normalized execution order, tool inventory and actual
transfer. The exact captured input predates adding visible people's numeric
HP to `execution.people`; those values remain mechanical parameters.

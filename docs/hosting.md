# Hosting

Run `below-the-crown` (or `below-the-crown host`) to open the host menu.
**Play** starts the current castle or joins an existing host in the selected data
directory, then opens a local player window. All players use SSH, including the
host's own character. Guests need no Codex installation.

The same menu provides Home, Settings, Saves, Diagnostics and Admin before and
during hosting. Arrows select or change values, Enter activates, Tab changes
pages, and Esc returns Home. Closing a player window disconnects only that player.
Leaving an attached menu keeps its host running. A menu-owned game has an explicit
**Save and stop** action with confirmation.

Play runs non-paid checks for Codex login, isolation, model metadata and network
availability. Failures stay visible with a suggested action and Details.
Diagnostics reruns the same checks. A missing desktop terminal produces clearly
labelled local connection instructions instead of a disappearing window.

## Commands

```sh
below-the-crown
below-the-crown host --help
below-the-crown manager
below-the-crown doctor
below-the-crown stop
```

Use `host --headless` with an explicit address and subnet for unattended operation:

```sh
below-the-crown host --headless --bind 192.168.100.25 --subnet 192.168.100.0/24 --language pl --scenario siege
```

Other host options are `--port`, `--max-players`, `--max-agents`,
`--npc-update-rate`, `--reasoning-effort`, `--save`, `--new`,
`--import-snapshot`, `--codex-bin`, `--sandbox-helper`, `--authorized-keys`
and `--audio`. Global `--data-dir` selects a separate castle.
`doctor --live` explicitly requests a paid model decision; ordinary checks do not.

## Settings

One HostSettings model owns startup defaults, CLI overrides, validation,
persistence, and the menu editor. Explicit CLI values override saved settings.
Settings persist in `server-settings.json`, outside world saves; new castles and
restores preserve them. Invalid edits or failed writes leave active settings intact.

Luna runs at the standard service tier. Reasoning effort defaults to high; the
editor offers only values advertised by the private provider-derived descriptor.
The current model advertises low, medium, high, xhigh and max. Discover the choices
in Diagnostics if they are not available yet. Effort applies to subsequent NPC and
speech-interpreter decisions; existing calls finish at their original effort and
NPC threads remain reusable.

The agent limit defaults to five, with a valid range of 1–32. It applies to NPCs
and speech interpretation together. Lowering it lets existing calls finish;
canceled calls hold their slots until worker cleanup completes.

Passive NPC thinking rate offers 1×/5×/15×, default 5×. It affects only idle
checks: 300, 60 or 20 active real seconds. Conversation, danger and action outcomes
wake a mind independently. The calendar always advances 24×: one game day lasts
60 active real minutes. Physiology uses the earlier 5× rate regardless of thinking
settings. Pauses freeze these clocks.

Language and scenario apply to a new or restored castle. Network and player
limit edits apply to the next host launch; displayed join commands continue to
describe the running listeners. Sound is local to the host menu; SSH stays silent.

## Network and privacy

An available private IPv4 LAN interface is selected automatically; Settings can
choose another interface or localhost. The normal port is 2222. Admission is
restricted to localhost and the selected interface's actual subnet.
`127.0.0.1` with `127.0.0.0/8` gives a localhost-only castle.

SSH exposes only a game PTY. Shell commands, file transfer, forwarding and remote
execution are rejected. The host key persists between runs. The automatic local
connection pins the game's private host-key file and ignores unrelated SSH
configuration. Optional public-key authentication further restricts admission;
it does not bypass the subnet restriction. Admin inspection and diagnostics remain
private to local IPC; players cannot access them.

A host started before an installation update continues running. The menu can join
it and use its existing controls. Save and restart it when ready to use newly
introduced settings; the installer never stops live sessions.

## State, saves and language

Data normally lives in `~/.local/share/below-the-crown`. Configuration and isolated
Codex credentials are private and are not save data. Never publish your state
directory or authentication files.

New players start in the selected scenario's dungeon using its prisoner template.
Entering a room starts a fresh visible transcript; returning shows none of what
happened while away. Private knowledge remains attached to each character.

Save entries show scenario, language and creation time. Restoring or creating a
castle validates the candidate first, saves the current branch, disconnects old
sessions, cancels obsolete inference and begins paused. **Resume and play** resumes
the world explicitly. Rejected saves leave the castle intact. Literal speech and
memories are never machine-rewritten when changing language.

Legacy format 1–3 JSON snapshots can be explicitly imported into separate native
storage. The old TypeScript application's data and sessions are not modified.
Shutdown does not advance an offline castle.

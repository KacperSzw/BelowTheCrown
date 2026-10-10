# Hosting

`below-the-crown host` opens setup, then the private manager. All player sessions
use SSH, including the host's own character. Guests need no Codex installation.

Setup chooses a new castle or save, a scenario, English/Polish, a private IPv4
LAN interface, and the maximum concurrent agents. The normal game port is 2222. Admission is restricted to localhost
and the selected interface's subnet. This release supports local networks only.

The manager's **Connect player** action opens a localhost SSH terminal when a
supported desktop terminal is available. Otherwise it shows the SSH command.
Closing a player terminal disconnects that character; it does not shut down the
server. The manager can inspect people, save, restore, pause and create a castle.

## Commands

```sh
below-the-crown host
below-the-crown host --help
below-the-crown manager
below-the-crown doctor
below-the-crown stop
```

Use `host --headless` with explicit settings for unattended operation:

```sh
below-the-crown host --headless --bind 192.168.100.25 --subnet 192.168.100.0/24 --language pl --scenario siege
```

`--bind` and `--subnet` are required in headless mode. `127.0.0.1` with
`127.0.0.0/8` gives a localhost-only castle. Other host options are `--port`,
`--max-players`, `--max-agents`, `--npc-update-rate`, `--save`, `--new`, `--import-snapshot`, `--codex-bin`,
`--sandbox-helper` and `--audio`. Global `--data-dir` works with every command.
Run `host --help` for argument details.
There is no player binary to distribute. A standard SSH client is enough.

## Agent limit

The default is five concurrent model decisions. Set `host --max-agents N` or
use left/right on **Maximum agents** in setup; valid limits are 1–32. In the
manager's Settings page, `[` decreases the limit and `]` increases it. The
manager shows the active call count beside the limit.

The change applies immediately to NPC decisions and player speech interpretation
together. Lowering it lets existing calls finish, then starts new work only when
capacity is available. Canceled calls hold their slot until their worker cleanup
finishes. A limit of one still allows background NPC work.

The setting persists as `maxAgents` in `server-settings.json`, alongside the
network settings. A CLI override takes precedence at startup. New castles and
save restoration preserve this host setting; it is not part of a saved world.

SSH exposes a game terminal only. Shell commands, file-transfer subsystems,
forwarding and remote execution are rejected. The host key persists between
runs. Public-key authentication can restrict admission further; it does not
bypass the allowed subnet.

## NPC thinking rate and time

Setup offers 1×/5×/15×, default 5×. In manager Settings, `,` and `.` cycle
this choice; headless hosts can use `--npc-update-rate 5`. It persists as
`npcUpdateRate` beside `maxAgents`; existing settings without it use 5×.

This affects only passive idle checks: 300, 60 or 20 active real seconds.
Conversation, danger and action outcomes wake a mind independently. Ordinary
thinking waits for short physical actions to finish; long duties can be answered
without restarting them. A canceled call retains its slot until cleanup ends.

The calendar always advances 24×: a day lasts 60 active real minutes. Hunger,
thirst, fatigue, rest and health effects use the earlier 5× physiology rate for
both players and NPCs, regardless of the thinking setting. Pauses freeze these
clocks. Changing the rate preserves saved needs, bodies and calendar time.

## State

Data lives beneath the user's XDG data directory, normally
`~/.local/share/below-the-crown`. `--data-dir` selects a separate castle.
Configuration and isolated Codex credentials stay private and are not save data.
Never publish your state directory or authentication files.

New players start in the selected scenario's dungeon using its prisoner template.
They choose a name and describe themselves through conversation. Character names
belong to humans and NPCs alike; the player UI does not show controller labels.

Entering a room starts a fresh local transcript. Returning shows none of what
happened while you were away. Private knowledge and remembered experiences remain
attached to the character.

## Saves and language

Save entries show their scenario, language and creation time. A restoration uses
the saved physical world and the selected restoration language. Changing language
in Settings applies to the next new/restored castle; literal speech and memories
are never machine-rewritten.

Restoring or creating a castle validates the candidate first, saves the current
branch, disconnects old sessions, cancels obsolete model work and begins paused.
A rejected save or package leaves the current castle intact.

A legacy format 1–3 JSON save can be explicitly imported into separate Rust data;
missing scenario identity means `castle`. The old TypeScript application's data
and active sessions are not modified by migration.

One running day is 60 real minutes. Pausing freezes the calendar; shutdown does
not advance an offline castle. Physical interaction deadlines remain readable
real-time windows. There is no speed setting.

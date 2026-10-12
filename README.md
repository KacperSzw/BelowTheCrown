# Below the Crown

**A ruthless medieval world, alive in your terminal.**

[![A first encounter in the dungeon](docs/images/gameplay.png)](docs/images/gameplay.png)

Escape the dungeon. Bargain, deceive, and turn rival loyalties to your advantage.
LLM-driven characters have their own memories, motives, and loyalties—and decide
what to say and do as the world unfolds.

Play solo or share the same castle with friends over LAN. Join from any computer
with SSH; only the host needs the game and a logged-in Codex CLI.

[![Talking with the dungeon guard](docs/images/dialogue.png)](docs/images/dialogue.png)

[![Two players sharing the same castle](docs/images/multiplayer.png)](docs/images/multiplayer.png)

## Host

Linux x64 or ARM64. Install [Codex CLI](https://developers.openai.com/codex/cli/)
and run `codex login`.

Download your binary from the [latest release](https://github.com/KacperSzw/BelowTheCrown/releases/latest).
Rename it to `below-the-crown`, then:

```sh
chmod +x below-the-crown
./below-the-crown host
```

Pick a scenario, English or Polish, your LAN interface, and the maximum number
of agents running at once (default 5), and passive NPC thinking rate (1×/5×/15×,
default 5×). A day lasts one active real hour at every thinking rate. The manager displays an SSH command for
everyone on that LAN. The agent limit can also be changed in manager Settings.

## Play

From any computer with an SSH client:

```sh
ssh -p 2222 192.168.100.25
```

Use the address shown by your host. No account, password or game installation
is needed. The host can join with `ssh -p 2222 127.0.0.1`.

Choose your name. Write to speak in your room; `/` shows actions.
Use CAPS or end with `!!` to shout to neighbouring rooms. `/auto on` interprets your speech as actions.

[Hosting and saves](docs/hosting.md) · [Troubleshooting](docs/sandbox.md) ·
[Development](docs/development.md) · [NPC minds](docs/npc-minds.md)

MIT · [Third-party notices](NOTICES.md)

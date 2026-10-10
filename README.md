# Below the Crown

A living castle in your terminal. Escape the dungeon. Bring down the king.
Everyone has their own motives. Anyone you meet could be another player.

## Host

Linux x64 or ARM64. Install [Codex CLI](https://developers.openai.com/codex/cli/)
and run `codex login`.

Download your binary from [Releases](https://github.com/KacperSzw/BelowTheCrown/releases).
Rename it to `below-the-crown`, then:

```sh
chmod +x below-the-crown
./below-the-crown host
```

Pick a scenario, English or Polish, and your LAN interface. The manager displays
an SSH command for everyone on that LAN.

## Play

From any computer with an SSH client:

```sh
ssh -p 2222 192.168.100.25
```

Use the address shown by your host. No account, password or game installation
is needed. The host can join with `ssh -p 2222 127.0.0.1`.

Choose your name. Write to speak in your room; `/` shows actions.
`/shout` reaches neighbouring rooms. `/auto on` interprets your speech as actions.

[Hosting and saves](docs/hosting.md) · [Troubleshooting](docs/sandbox.md) ·
[Development](docs/development.md)

MIT · [Third-party notices](NOTICES.md)

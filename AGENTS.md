# Below the Crown

The engine owns every physical fact. Dialogue, notes, beliefs, and model output cannot grant items, move actors, or resolve outcomes. Player and NPC actions use the same validator.

NPC knowledge is private. Bind identity to scheduler-owned threads. Preserve witnessed/heard versus inferred information. Save/load restores minds and cancels obsolete inference.

Production decisions use gpt-6-luna/high at the standard service tier. Fixtures belong in tests; never introduce a scripted production fallback.

Codex workers inherit no tools or integrations. Saves, host sockets, and other minds stay outside their filesystem. Never log or commit credentials.

Use Rust, SQLite, and a single authoritative simulation owner. Run cargo test and cargo clippy after behavior changes. Tests protect contracts rather than designer tuning.

Player connections use localhost or LAN SSH. The private manager uses local IPC. Preserve terminal focus and never stop another live session automatically.

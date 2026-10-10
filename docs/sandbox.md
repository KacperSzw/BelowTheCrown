# Codex and sandbox troubleshooting

Run `below-the-crown doctor` before hosting. The game needs a compatible,
logged-in Codex CLI and Linux permission to create an unprivileged user/mount
namespace. The downloaded game embeds its own bubblewrap helper; installing
bubblewrap separately is unnecessary.

Native, Nix and npm/pnpm Codex installations are supported. The host resolves the
native Codex payload before entering isolation. Game workers do not inherit
local tools, MCP integrations or normal Codex configuration.

If your login is file-backed, setup imports credentials into a private game
worker directory. If it exists only in a keyring, follow the private login
command displayed by `doctor`. The game does not weaken authentication or copy
credentials into a save. See [Codex authentication](https://developers.openai.com/codex/auth/).

## Namespace restrictions

Some Linux installations disable unprivileged user namespaces. Ubuntu 24.04 and
later can additionally restrict them through AppArmor. The game reports the
failed probe and stops model execution; it never falls back to unsandboxed work.

An administrator can allow this particular helper with a narrow local AppArmor
profile. Use the exact extracted helper path printed by `doctor`, for example:

```text
abi <abi/4.0>,
include <tunables/global>

profile below-the-crown-helper /absolute/path/from/doctor flags=(unconfined) {
  userns,
}
```

Save it as `/etc/apparmor.d/below-the-crown-helper` and load it with
`sudo apparmor_parser -r /etc/apparmor.d/below-the-crown-helper`. This profile
permits namespace creation for the named helper; bubblewrap still constructs the
worker's restricted filesystem and process environment. Apply it only on systems
where AppArmor blocks the probe. The game does not edit system policy for you.

See the [Ubuntu namespace restrictions](https://documentation.ubuntu.com/release-notes/24.04/#unprivileged-user-namespace-restrictions)
and [bubblewrap documentation](https://github.com/containers/bubblewrap).

## Rebuilding the distributed helper

The repository pins bubblewrap 0.13.0 and its source hash in `flake.nix`.
`flake.lock` pins the compiler/toolchain package sources. Source releases include
the exact bubblewrap and libcap archives plus this recipe and notices.

```sh
nix build .#sandbox-helper
./result/bin/bwrap --version
nix build .#helper-sources
```

Build natively on the target architecture. The derivation uses a musl static C
compiler, static libcap, Meson/Ninja, and disables SELinux, documentation,
completions and helper tests. It applies no modifications to bubblewrap itself.
There are no runtime paths to Nix store files in the resulting helper.

The helper is a separate executable extracted into a private cache and invoked
as a process, not a linked game library. Its LGPL license remains its own. You
may build a replacement and select the explicit helper override supported by
`doctor`; a target-matched, working sandbox is still required. Release builds
must embed a target-matched static helper.

---
name: remote-build
description: Run cargo build/test/clippy for this Zed fork on the fast Windows machine over SSH instead of compiling locally. Triggers on "build this", "run the tests", "cargo test", when working from the slow Linux machine.
---

# Remote build (Windows machine over SSH)

The Linux machine is for editing; it compiles this fork far too slowly. The Windows machine builds
at 16 threads. `script/winrun` bridges the two: it pushes the current commit, checks that exact SHA
out on the Windows machine, runs the command there, and streams the output back with the exit code
propagated.

Requires `~/.config/zed-winrun.env` (outside the repo, since it holds machine-specific paths):

```sh
ZED_WIN_HOST=zed-win                    # a Host entry in ~/.ssh/config
ZED_WIN_REPO='C:\Users\you\src\zed'     # the Zed clone on the Windows machine
```

## What to delegate

| Command | Where |
| --- | --- |
| `cargo check -p <crate>` | **locally** — about 4 minutes here, and the round-trip isn't worth it |
| `cargo test`, `cargo build`, `script/clippy` | `script/winrun` |
| `cargo build --release -p zed`, release packaging | `script/winrun --detach` |

```bash
script/winrun -- cargo test -p agent_ui ticket_metadata_store
script/winrun --detach -- cargo build --release -p zed   # returns immediately
script/winrun --tail                                      # follow the detached run
script/winrun --status                                    # done? exit code?
script/winrun --no-sync -- git rev-parse HEAD             # inspect without syncing
```

Use `--detach` for anything past roughly ten minutes: it runs the command through a detached
`cmd.exe`, so the build survives an SSH drop. A plain `winrun` dies with the connection.

## Traps

- **Commit first.** `winrun` refuses to run with a dirty working tree, because the Windows machine
  builds what was pushed — a build of un-pushed edits would be a lie. Commit, then delegate.
- **The Windows machine ends up on a detached HEAD** at the SHA that was built. That is deliberate:
  it makes "what exactly did we compile" answerable. Don't "fix" it by checking out a branch there.
- **No GUI over SSH.** Commands from sshd run outside the interactive desktop session, so the
  `run-zed` skill (`driver.ps1 launch` / `click` / `screenshot`) **cannot** work through `winrun`:
  the window never appears on the desktop and UI Automation finds nothing. Visual UI testing still
  needs an agent running on the Windows desktop itself. Build and test remotely; click locally.
- **PATH in a non-interactive session.** If `cargo` is "not recognized" remotely while it works in
  the developer's own terminal, rustup is only on the interactive profile's PATH — fix the machine
  PATH on Windows rather than working around it in the script.
- The command is embedded into a PowerShell here-string, so a command containing single quotes needs
  care. Cargo invocations don't; complex one-liners might.

---
name: remote-build
description: Run cargo build/test/clippy for this Zed fork on the fast Windows machine over SSH instead of compiling locally. Triggers on "build this", "run the tests", "cargo test", when working from the slow Linux machine.
---

# Remote build (Windows machine over SSH)

The Linux machine is for editing; it compiles this fork far too slowly. The Windows machine builds
at 16 threads. `script/winrun` bridges the two: it pushes the current commit, checks that exact SHA
out on the Windows machine, runs the command there, and streams the output back with the exit code
propagated.

Requires `~/.config/zed-winrun.env`. It lives outside the repo on purpose — this is a public fork,
and the host name and paths belong to the developer's machines, not to the project:

```sh
ZED_WIN_HOST=zed-win                    # a Host entry in ~/.ssh/config
ZED_WIN_REPO='C:\Users\you\src\zed'     # the Windows clone
ZED_WSL_REPO=/home/you/zed              # the clone inside WSL      (--wsl only)
ZED_WSL_TARGET=/home/you/zed-target     # CARGO_TARGET_DIR in WSL   (--wsl only)
```

The transport is SSH to the Windows machine's OpenSSH Server over Tailscale, key-only, with the
port opened on the Tailscale interface alone. Setting that up is a one-off; `script/winrun --help`
restates what the configuration file needs.

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

## The dev loop: build a Linux binary and run it here

`--wsl` runs in the clone inside the Windows machine's WSL distribution, so the output is a Linux
binary that runs on this laptop. That makes the edit-test loop a few minutes instead of a release
round-trip — no tag, no publish, no manual download.

```bash
git commit -am "..."                                       # winrun refuses a dirty tree
script/winrun --wsl --detach -- cargo build -p zed         # dev profile, incremental
script/winrun --wsl --status                               # or --wsl --tail
script/winrun --wsl -- 'strip --strip-debug \
    $ZED_WSL_TARGET/debug/zed -o $HOME/zed-dev'            # shrink before the transfer
script/winrun --wsl --pull '$HOME/zed-dev' ./zed-dev && chmod +x ./zed-dev
./zed-dev
```

Stage artifacts under `$HOME` in WSL, not `/tmp`: the distribution is shut down and restarted
between winrun invocations, and `/tmp` does not survive it.

Measured on this setup: an incremental `cargo build -p zed` takes about 2 minutes, the stripped dev
binary is 1.1 GB, and pulling it takes 38 seconds — md5 identical either side.

Build with the **dev** profile for iteration. `[profile.release]` carries `lto = "thin"` and
`codegen-units = 1`, which is minutes per iteration for no benefit while testing behaviour; keep it
for actual releases.

`--pull` is binary-safe. It has to be: PowerShell re-encodes what native commands write to stdout,
so piping a binary through `wsl -e cat` corrupts it. The file is staged onto the Windows filesystem
and carried by scp over the SFTP subsystem, which never touches a shell.

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
- **`cargo.exe` found but refusing to run** ("no application is associated with the specified file",
  or from cmd "the system cannot execute the specified program") is a different problem: rustup's
  shims in `%USERPROFILE%\.cargo\bin` are then 0-byte symlinks to `rustup.exe`, which do not resolve
  in an SSH session. Replace them with copies — rustup dispatches on the executable's name, so a
  copy named `cargo.exe` *is* cargo. A later `rustup self update` can bring the symlinks back.

  ```powershell
  $bin = "$env:USERPROFILE\.cargo\bin"
  Get-ChildItem $bin -Filter *.exe | Where-Object { $_.Length -eq 0 } | ForEach-Object {
      $t = $_.FullName; Remove-Item $t -Force; Copy-Item "$bin\rustup.exe" $t
  }
  ```
- The command is embedded into a PowerShell here-string, so a command containing single quotes needs
  care. Cargo invocations don't; complex one-liners might.

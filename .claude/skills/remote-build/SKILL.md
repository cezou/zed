---
name: remote-build
description: Run cargo build/test/clippy for this Zed fork on the fast Windows machine over SSH, for when the developer picks that machine over this one. Triggers on "build this", "run the tests", "cargo test".
---

# Remote build (Windows machine over SSH)

**Ask which machine before compiling.** Never assume. Every build, test, or clippy run starts with
the question — Windows (this skill) or this Linux machine? — because the answer changes with what
else the developer is doing, and only they know. State the trade-off in the question so the choice
is informed: Windows is 16 threads and leaves this laptop free, but the binary has to travel back;
building locally needs `-j` chosen against the RAM and free disk available at that moment (`nproc`,
`free -h`, `df -h .`) and will compete with everything else they have open.

The Linux machine is for editing and compiles this fork slowly. The Windows machine builds
at 16 threads. `script/winrun` bridges the two: it carries the current commit over as a git bundle
over SSH, checks that exact SHA out on the Windows machine, runs the command there, and streams the
output back with the exit code propagated.

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

Once the developer has picked Windows, this is how the work splits:

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
script/winrun --wsl --detach -- cargo build --profile release-fast -p zed
script/winrun --wsl --status                               # or --wsl --tail
script/winrun --wsl -- 'strip --strip-debug \
    $ZED_WSL_TARGET/release-fast/zed -o $HOME/zed-rf'      # shrink before the transfer
script/winrun --wsl --pull '$HOME/zed-rf' ./zed-rf && chmod +x ./zed-rf
pkill -x zed-rf                                            # never `pkill zed`
./zed-rf
```

Build **`release-fast`** (`inherits = "release"`, no LTO, 16 codegen units), not the dev profile.
Only a release profile turns `debug_assertions` off, which is what makes `rust-embed` actually embed
the assets — a dev binary reads them off disk at the build machine's path and needs the bind mount
below. `release-fast` is self-contained and runs from anywhere. A full build measured 11m30 on this
setup, and the stripped binary is 580 MB against 1.1 GB for the dev one.

Close the running instance before launching the new one. This fork's channel is `dev`
(`crates/zed/RELEASE_CHANNEL`), so `main.rs` skips the single-instance check and every launch opens a
*new* window — and both instances then write the same `db/0-dev` SQLite, clobbering each other's
tickets and sessions. Match the binary's exact name (`pkill -x zed-rf`): `pkill zed` matches every
other build lying around.

Stage artifacts under `$HOME` in WSL, not `/tmp`: the distribution is shut down and restarted
between winrun invocations, and `/tmp` does not survive it.

Measured on this setup: a full `release-fast` build takes 11m30 and its stripped binary is 580 MB;
an incremental one takes about 8 minutes. The dev profile builds incrementally in about 2 minutes but
produces 1.1 GB that takes 38 seconds to pull — and cannot be run here at all (see the traps). So the
loop costs roughly 8 minutes of build plus half a minute of transfer per iteration; batch several
edits into one build rather than one per change. `--pull` is md5-identical either side.

Never `[profile.release]` for iteration: it carries `lto = "thin"` and `codegen-units = 1`, which is
minutes per iteration for no benefit while testing behaviour. Keep it for actual releases.

`--pull` is binary-safe. It has to be: PowerShell re-encodes what native commands write to stdout,
so piping a binary through `wsl -e cat` corrupts it. The file is staged onto the Windows filesystem
and carried by scp over the SFTP subsystem, which never touches a shell.

## Traps

- **A dev-profile binary is not relocatable, and a symlink will not fix it.** `rust-embed` embeds
  nothing in a debug build: it reads the assets off disk at runtime, at the absolute path baked in at
  compile time. A binary built in the WSL clone therefore looks for `$ZED_WSL_REPO/assets` on *this*
  machine and panics on `settings/default.json` if that path is not there. Make the build path exist
  here with a **bind mount**:

  ```sh
  sudo mkdir -p "$ZED_WSL_REPO"
  sudo mount --bind "$PWD" "$ZED_WSL_REPO"     # add to /etc/fstab to survive a reboot
  ```

  It has to be a bind mount, not a symlink. The generated `get()` guards against path traversal with
  `file_path.canonicalize().starts_with(<folder path canonicalized at build time>)`, and
  `canonicalize` resolves symlinks — so through a symlink the path comes back as the *real* repo path,
  fails the prefix check, and the lookup returns `None`. Its only fallback accepts a final component
  that is itself a symlink, which an asset file is not. A bind mount is neither a symlink nor a `..`,
  so the path stays under `$ZED_WSL_REPO` and the check passes.

  `winrun` checks the same SHA out on both sides, so the assets match. The alternative — enabling
  `rust-embed`'s `debug-embed` feature — makes the binary self-contained but slows every incremental
  build, which is why Zed leaves it off.

- **Commit first.** `winrun` refuses to run with a dirty working tree, because the Windows machine
  builds a commit — a build of uncommitted edits would be a lie. Commit, then delegate.
- **Nothing is pushed to the remote.** The commit travels as a bundle of whatever is not yet on
  `origin`, staged under `target/` on the Windows side and fetched into `refs/winrun/sync` there.
  Pushing was the old mechanism; it broke on scratch worktree branches that get rebased locally
  (non-fast-forward) and littered the shared remote with build-only branches.
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

---
name: release
description: Cut a release of this Zed fork — build, package, tag, publish on GitHub. Triggers on "make a release" / "cut a release" / "ship a release".
---

# Release

1. **Merge** the feature branch into `main` if not already: `git merge --ff-only <branch>`.
2. **Verify**: `cargo check -p zed` and `cargo test -p <touched crates>` clean.
3. **Build**: `CARGO_BUILD_JOBS=<N> cargo build --release -p zed`.

   Pick `N` from what the machine actually has free *right now*, don't hardcode it:
   ```powershell
   # Windows
   (Get-CimInstance Win32_Processor).NumberOfLogicalProcessors
   [math]::Round((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory/1MB,1)  # GB free
   ```
   ```bash
   # Linux / WSL
   nproc; free -g | awk 'NR==2{print $7" GB available"}'
   ```
   Default to the logical-processor count (16 on this machine) — don't be shy. Only drop below it
   if free RAM is tight: budget roughly 1 GB per job for the compile phase, and leave several GB
   headroom for the final link, which is single-threaded and the memory peak of the whole build.

4. **Package** — this fork ships both a Linux and a Windows artifact.

   **Do not run the two builds concurrently.** WSL2 has no CPU of its own; it shares the host's
   physical cores. Two builds at `N` jobs each is `2N` tasks on the same cores — no extra compute,
   double the peak RAM, and interleaved logs. Build them one after the other at full `N`.

   Linux (from WSL, with the target dir on ext4 — building into `/mnt/c` is far slower):
   ```bash
   CARGO_TARGET_DIR=$HOME/zed-target-linux CARGO_BUILD_JOBS=<N> cargo build --release -p zed
   strip --strip-debug $HOME/zed-target-linux/release/zed -o zed
   tar -czf zed-linux-x86_64-<name>.tar.gz zed LICENSE-GPL LICENSE-APACHE
   ```

   Windows (native PowerShell — `strip`/`tar.gz` are not the platform convention here):
   ```powershell
   $env:CARGO_BUILD_JOBS = '<N>'; cargo build --release -p zed
   Compress-Archive -Path target\release\zed.exe, LICENSE-GPL, LICENSE-APACHE `
       -DestinationPath zed-windows-x86_64-<name>.zip -Force
   ```

   The Windows **release** build needs `cmake` — `wasmtime-c-api-impl`'s build script spawns it,
   and the debug build does not pull that crate, so this only bites here. It is not on PATH but
   Visual Studio ships one; prepend it rather than installing anything:
   ```
   C:\Program Files\Microsoft Visual Studio\18\Community\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin
   ```
   (Same missing `cmake` is why `./script/clippy`, which passes `--release --all-features`, cannot
   run on this machine; `cargo clippy --workspace --all-targets` is the working substitute.)
5. **Tag**: `git tag -a v<zed-version>-<name> -m "<summary>"`.
6. **Push the tag** — not `main` (never force-push `main` yourself; that stays the user's call):
   ```
   git push git@github.com:cezou/zed.git <tag>   # SSH avoids HTTPS credential prompts
   ```
7. **Publish** — both artifacts on the one release:
   ```
   gh release create <tag> \
       zed-linux-x86_64-<name>.tar.gz zed-windows-x86_64-<name>.zip \
       --repo cezou/zed --title "<title>" --notes-file <notes.md>
   ```

Before writing release notes: scrub for secrets/PII (emails, names, internal IDs, tokens).

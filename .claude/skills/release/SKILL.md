---
name: release
description: Cut a release of this Zed fork — build, package, tag, publish on GitHub. Triggers on "make a release" / "cut a release" / "ship a release".
---

# Release

1. **Merge** the feature branch into `main` if not already: `git merge --ff-only <branch>`.
2. **Verify**: `cargo check -p zed` and `cargo test -p <touched crates>` clean.
3. **Build**: `CARGO_BUILD_JOBS=<N> cargo build --release -p zed`. Start at 4 (this machine has crashed at full parallelism before); check `free -h` / `uptime` and raise `N` if RAM and load allow.
4. **Package**:
   ```
   strip --strip-debug target/release/zed -o zed
   tar -czf zed-linux-x86_64-<name>.tar.gz zed LICENSE-GPL LICENSE-APACHE
   ```
5. **Tag**: `git tag -a v<zed-version>-<name> -m "<summary>"`.
6. **Push the tag** — not `main` (never force-push `main` yourself; that stays the user's call):
   ```
   git push git@github.com:cezou/zed.git <tag>   # SSH avoids HTTPS credential prompts
   ```
7. **Publish**:
   ```
   gh release create <tag> zed-linux-x86_64-<name>.tar.gz --repo cezou/zed --title "<title>" --notes-file <notes.md>
   ```

Before writing release notes: scrub for secrets/PII (emails, names, internal IDs, tokens).

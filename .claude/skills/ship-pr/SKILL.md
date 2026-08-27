---
name: ship-pr
description: Finalize work and ship it — verify comments, error handling, tests, fmt, clippy, then commit + push + PR. Triggers when the user says "c'est bon, MR", "c'est bon, PR" or "vasy push".
license: Proprietary
---

# Ship PR

Run when the user says **"c'est bon, MR"**, **"c'est bon, PR"** or **"vasy push"**. Do every step, in order.

1. **Doc comments** — `///` rustdoc on items worth documenting (public ones especially), `//!` for a module header. Doc comments on actions are shown to users, so write them for users.
2. **Comments inside bodies** — allowed only to explain **why**, when the reason is tricky or non-obvious. Delete anything that merely summarizes or organizes the code. If a comment explains *what* a function does, it belongs in its `///` instead.
3. **Imports** — `use` at the top of the file, never mid-function. `cargo fmt` groups them; don't hand-order.
4. **Error handling** — no `unwrap()`/`expect()` in shipped paths; propagate with `?`. Never `let _ =` on a fallible call: propagate, or `.log_err()` when the error is genuinely ignorable. Check indexing that could panic. Async failures must reach the UI layer so the user sees them.
5. **File layout** — no `mod.rs`; new crates set `[lib] path = "…"` to a descriptive name. Prefer extending an existing file over adding a small new one.
6. **Unit tests** — add/adapt them. `cargo test -p <crate>` (or `cargo nextest run -p <crate>`). In GPUI tests use `cx.background_executor().timer(…)`, never `smol::Timer::after`.
7. **fmt + clippy** — `cargo fmt --all`, then `./script/clippy -p <crate>` (`script/clippy.ps1` on Windows). Both must be clean; clippy runs `--deny warnings`. It builds `--release`, so on Windows put VS's cmake on `PATH` first or `wasmtime-c-api-impl` fails.
8. **Commit** — one commit (squash if >1). **Not** Conventional Commits: imperative, capitalized title, optional `crate_name:` prefix, no trailing period. End with `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`.
9. **Push + PR** — `gh pr create`, or update if one exists (`gh pr list --repo cezou/zed --head <branch>`). Never on `main`.
   - **Always pass `--repo cezou/zed --base main` explicitly.** This repo is a fork of `zed-industries/zed`, and `gh` resolves a fork's default PR target to the **upstream** — so a bare `gh pr create` opens a public pull request against Zed itself. Same for every other `gh` call here: `gh pr list/view/merge --repo cezou/zed`. `gh repo set-default` does **not** override a fork's PR-target resolution; the flag does.

## Commit & PR text

English. Least text for most concision. Trim hard.

- **Commit**: title line + terse bullets only if they add info.
- **PR title**: same rules as the commit title — imperative, capitalized, no `fix:`/`feat:` prefix, no trailing punctuation. Optional crate scope: `git_ui: Add history view`.
- **PR description**: a few bullets of what changed and why. No fixed template, no "Context/What's the problem" sections, no "you asked for".
- **Release Notes** — required, and the **last** section. Blank line after the heading, exactly one bullet: `- Added …` / `- Fixed …` / `- Improved …` for user-facing changes, `- N/A` for docs-only and other non-user-facing ones.

```
Release Notes:

- N/A
```

- **`.rules` additions** — never edit `.rules` or `CLAUDE.md` inline during feature work (they are the same file; `CLAUDE.md` is a symlink). If the work surfaced a non-obvious, repeatedly-hit, actionable trap, propose it under a **"Suggested .rules additions"** heading in the PR description and let the reviewer decide.
- **Schema/diagram**: add a Mermaid `flowchart TD` or `sequenceDiagram` ONLY if the logic is genuinely complex (multi-step pipeline, branching lifecycle) and a diagram earns its place. Skip it otherwise. Keep labels simple so GitHub parses them.

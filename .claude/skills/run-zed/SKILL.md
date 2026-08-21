---
name: run-zed
description: Launch and drive this Zed fork in dev mode on native Windows to click UI elements, screenshot the window, and verify UI changes. Triggers on "run zed", "test this UI change", "screenshot zed", "click the X button and check".
---

# Run Zed (Windows-native, UI Automation-driven)

Must run under native Windows PowerShell, not WSL. Under WSL2/WSLg, Zed's GPU-composited
window can't be captured by X11 screenshot tools (comes back solid black), and there's no
input-simulation tool available there either. Native Windows has neither problem.

Clicking goes through UI Automation (backed by GPUI's real AccessKit accessibility tree),
not blind screen coordinates — it survives window moves/resizes and doesn't depend on
guessing pixel positions.

1. **Build**: `.\.claude\skills\run-zed\driver.ps1 build`
2. **Launch** (isolated, stateless — never touches the developer's real Zed config):
   `.\.claude\skills\run-zed\driver.ps1 launch`
3. **Discover what's clickable**: `.\.claude\skills\run-zed\driver.ps1 list-elements`
   - Text buttons ("Save", "Cancel", …) have an accessible `Name` and are clickable by
     `-Name` directly.
   - Icon-only buttons mostly do **not** have a `Name` set today. For those, find the
     right one by `Role` and its position in the JSON dump, then click with
     `-Role Button -Index N`.
   - If an interactive element you expect doesn't show up in the dump at all (not just
     unnamed — genuinely absent), that's a missing `.role(...)` on that component in the
     Rust source, not a bug in this driver — fix it there rather than debugging the script.
4. **Click**: `.\.claude\skills\run-zed\driver.ps1 click -Name "Save"`
   or `.\.claude\skills\run-zed\driver.ps1 click -Role Button -Index 2`
5. **Screenshot**: `.\.claude\skills\run-zed\driver.ps1 screenshot -Out shot.png`
6. **Look at the screenshot** with the Read tool and confirm it actually shows the
   expected state — a non-zero exit code from `click`/`screenshot` is not itself proof
   the UI change works; the screenshot must be inspected.
7. **Cleanup**: `.\.claude\skills\run-zed\driver.ps1 quit`

Definition of done: a screenshot was taken **and** visually confirmed to show the
expected state, not just "the command exited 0".

## Exit codes

`build`/`launch` fail loudly with a non-zero code and a message on the actual error
(compile failure, window never appeared, etc.). `click`/`screenshot` distinguish "no
matching element" from "matched but couldn't invoke it" from "captured a blank/uniform
image" — read the error message rather than only checking the exit code, since the fix
differs (adjust the selector vs. investigate rendering vs. add a `.role()`/`.aria_label()`
to the component).

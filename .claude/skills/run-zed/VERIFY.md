# Verify run-zed driver

Run from native Windows PowerShell, in repo root. Report pass/fail per step, with output/errors for any failure. Do not assume success — read actual output.

```
.\.claude\skills\run-zed\driver.ps1 build
```
Pass: exit 0.

```
.\.claude\skills\run-zed\driver.ps1 launch
```
Pass: prints `launched pid=... hwnd=...`, hwnd non-zero.

```
.\.claude\skills\run-zed\driver.ps1 list-elements
```
Run twice, ~2s apart. Pass: valid JSON array both times; second run has more elements than a bare root (proves tree populated, not just root node).

```
.\.claude\skills\run-zed\driver.ps1 click -Name "<pick a text button from the dump>"
.\.claude\skills\run-zed\driver.ps1 click -Role Button -Index <pick an icon button index>
```
Pass: both exit 0, each produces an observable UI change.

```
.\.claude\skills\run-zed\driver.ps1 screenshot -Out shot.png
```
Pass: exit 0, `shot.png` exists, not solid-color (driver already checks this and fails loudly if so). Open/view the image and confirm it shows Zed's UI, not black/blank.

```
.\.claude\skills\run-zed\driver.ps1 quit
```
Pass: process exits; `%APPDATA%\Zed` unchanged (was `ZED_STATELESS=1`).

## Report

For each step: pass/fail + exact error text if failed. If a step fails, stop and report — don't work around it silently.

<#
.SYNOPSIS
  Drives this Zed fork on native Windows for agent-run UI testing: build, launch,
  enumerate the UI Automation tree (backed by GPUI's AccessKit integration), click
  elements by accessible name/role, screenshot the window, and quit.

.DESCRIPTION
  Clicking goes through UI Automation (InvokePattern), not blind screen coordinates,
  because GPUI exposes a real accessibility tree via AccessKit. This survives window
  moves/resizes and doesn't depend on pixel-perfect coordinate guessing.

  Two things to know before using this:
  - GPUI only builds the accessibility tree lazily, on the first UI Automation query
    (Windows' WM_GETOBJECT). The very first query after launch returns only a root
    node; the populated tree lands asynchronously on the next frame. list-elements
    retries automatically for this reason.
  - Text buttons (ui::Button) get an accessible Name automatically from their visible
    label. Icon-only buttons (ui::IconButton) mostly do NOT have a Name set today.
    For those, match by -Role and -Index (position among matches) using the bounds
    reported by list-elements, not by -Name.

.EXAMPLE
  .\driver.ps1 build
  .\driver.ps1 launch
  .\driver.ps1 list-elements
  .\driver.ps1 click -Name "Save"
  .\driver.ps1 click -Role Button -Index 3
  .\driver.ps1 screenshot -Out shot.png
  .\driver.ps1 quit
#>

param(
    [Parameter(Position = 0, Mandatory = $true)]
    [ValidateSet('build', 'launch', 'list-elements', 'click', 'screenshot', 'quit', 'focus', 'keys', 'paste')]
    [string]$Command,

    [string]$Name,
    [string]$AutomationId,
    [string]$Role,
    [int]$Index = 0,
    [string]$Out,
    [string]$Text,
    [int]$TimeoutSeconds = 10,
    [switch]$Persistent
)

$ErrorActionPreference = 'Stop'

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot '..\..\..')
$StateFile = Join-Path $PSScriptRoot '.run-zed-state.json'

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

if (-not ([System.Management.Automation.PSTypeName]'RunZed.Native').Type) {
    Add-Type -Namespace RunZed -Name Native -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, int dx, int dy, uint dwData, IntPtr dwExtraInfo);
[DllImport("user32.dll")] public static extern bool IsWindow(IntPtr hwnd);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
[DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hwnd, int nCmdShow);

public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
'@
}

# Exit codes an agent can branch on.
$ExitOk = 0
$ExitNoState = 10
$ExitWindowGone = 11
$ExitElementNotFound = 12
$ExitClickFailed = 13
$ExitScreenshotFailed = 14
$ExitBuildFailed = 15
$ExitLaunchFailed = 16

function Save-State($state) {
    $state | ConvertTo-Json | Set-Content -Path $StateFile
}

function Load-State {
    if (-not (Test-Path $StateFile)) {
        Write-Error "No run-zed state found. Run '.\driver.ps1 launch' first."
        exit $ExitNoState
    }
    Get-Content -Path $StateFile -Raw | ConvertFrom-Json
}

function Build-Zed {
    Push-Location $RepoRoot
    try {
        cargo build -p zed
        if ($LASTEXITCODE -ne 0) {
            Write-Error "cargo build -p zed failed with exit code $LASTEXITCODE"
            exit $ExitBuildFailed
        }
    } finally {
        Pop-Location
    }
}

function Launch-Zed {
    $binary = Join-Path $RepoRoot 'target\debug\zed.exe'
    if (-not (Test-Path $binary)) {
        Write-Error "$binary does not exist. Run '.\driver.ps1 build' first."
        exit $ExitLaunchFailed
    }

    # Isolated, disposable state so a test run never touches the developer's real
    # Zed config/database; ZED_ALLOW_EMULATED_GPU covers VMs/CI boxes whose GPU
    # shows up as software-emulated to Zed's Vulkan backend.
    #
    # ZED_STATELESS swaps the SQLite database for an in-memory one
    # (crates/db/src/db.rs, open_db), so anything under test that must survive a
    # restart — recorded ticket worktrees, Claude session ids — is silently lost.
    # -Persistent opts out for exactly those scenarios.
    if ($Persistent) {
        Remove-Item Env:\ZED_STATELESS -ErrorAction SilentlyContinue
    } else {
        $env:ZED_STATELESS = '1'
    }
    $env:ZED_ALLOW_EMULATED_GPU = '1'

    $proc = Start-Process -FilePath $binary -PassThru

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $proc.Refresh()
        if ($proc.HasExited) {
            Write-Error "Zed process exited immediately (exit code $($proc.ExitCode))."
            exit $ExitLaunchFailed
        }
        if ($proc.MainWindowHandle -ne [IntPtr]::Zero) {
            Save-State @{ Pid = $proc.Id; Hwnd = [int64]$proc.MainWindowHandle }
            Write-Output "launched pid=$($proc.Id) hwnd=$($proc.MainWindowHandle)"
            return
        }
        Start-Sleep -Milliseconds 200
    }

    Write-Error "Zed window did not appear within ${TimeoutSeconds}s."
    exit $ExitLaunchFailed
}

function Get-ZedHwnd {
    $state = Load-State
    $hwnd = [IntPtr]$state.Hwnd
    if (-not [RunZed.Native]::IsWindow($hwnd)) {
        Write-Error "Zed window (hwnd=$hwnd) no longer exists. Launch again."
        exit $ExitWindowGone
    }
    $hwnd
}

function Get-DescendantNodes($root) {
    $walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
    $nodes = New-Object System.Collections.Generic.List[object]

    function Walk($el, $depth) {
        if ($depth -gt 25) { return }
        $child = $walker.GetFirstChild($el)
        while ($null -ne $child) {
            $nodes.Add($child)
            Walk $child ($depth + 1)
            $child = $walker.GetNextSibling($child)
        }
    }

    Walk $root 0
    $nodes
}

function Get-ElementInfo($el) {
    $invokeAvailable = $false
    $pattern = $null
    if ($el.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$pattern)) {
        $invokeAvailable = $true
    }
    $rect = $el.Current.BoundingRectangle
    [pscustomobject]@{
        Name           = $el.Current.Name
        AutomationId   = $el.Current.AutomationId
        Role           = $el.Current.ControlType.ProgrammaticName -replace '^ControlType\.', ''
        BoundingRect   = [pscustomobject]@{
            X = $rect.X; Y = $rect.Y; Width = $rect.Width; Height = $rect.Height
        }
        InvokeAvailable = $invokeAvailable
    }
}

# GPUI/AccessKit builds its tree lazily on the first UI Automation query, and the
# populated tree only lands on the frame after that. Poll instead of trusting a
# single read.
function Get-PopulatedNodes($root) {
    $deadline = (Get-Date).AddSeconds(5)
    $nodes = Get-DescendantNodes $root
    while ($nodes.Count -le 1 -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 300
        $nodes = Get-DescendantNodes $root
    }
    $nodes
}

function List-Elements {
    $hwnd = Get-ZedHwnd
    $root = [System.Windows.Automation.AutomationElement]::FromHandle($hwnd)
    $nodes = Get-PopulatedNodes $root
    $nodes | ForEach-Object { Get-ElementInfo $_ } | ConvertTo-Json -Depth 4
}

function Find-Element {
    $hwnd = Get-ZedHwnd
    $root = [System.Windows.Automation.AutomationElement]::FromHandle($hwnd)
    $nodes = Get-PopulatedNodes $root

    # Force an array even when exactly one item matches - PowerShell unwraps a
    # single-item pipeline result to a scalar, which would make plain .Count reads
    # unreliable below (e.g. AutomationElement has no .Count of its own).
    $candidates = @($nodes | Where-Object {
        ($null -eq $Name -or $Name -eq '' -or $_.Current.Name -eq $Name) -and
        ($null -eq $AutomationId -or $AutomationId -eq '' -or $_.Current.AutomationId -eq $AutomationId) -and
        ($null -eq $Role -or $Role -eq '' -or ($_.Current.ControlType.ProgrammaticName -replace '^ControlType\.', '') -eq $Role)
    })

    if ($candidates.Count -le $Index) {
        return $null
    }
    $candidates[$Index]
}

function Click-Element {
    $el = Find-Element
    if ($null -eq $el) {
        Write-Error "No element matched Name='$Name' AutomationId='$AutomationId' Role='$Role' Index=$Index. Run 'list-elements' to see what's available."
        exit $ExitElementNotFound
    }

    $pattern = $null
    if ($el.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$pattern)) {
        $pattern.Invoke()
        Write-Output "clicked (invoke pattern): $($el.Current.Name)"
        return
    }

    # Fallback for nodes with no invoke pattern: click the geometric center of the
    # UIA-reported bounding rect (still element-driven, not a guessed coordinate).
    $rect = $el.Current.BoundingRectangle
    if ($rect.IsEmpty) {
        Write-Error "Element has no invoke pattern and no bounding rectangle to click."
        exit $ExitClickFailed
    }
    $cx = [int]($rect.X + $rect.Width / 2)
    $cy = [int]($rect.Y + $rect.Height / 2)
    [RunZed.Native]::SetCursorPos($cx, $cy) | Out-Null
    Start-Sleep -Milliseconds 50
    [RunZed.Native]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero) # MOUSEEVENTF_LEFTDOWN
    Start-Sleep -Milliseconds 50
    [RunZed.Native]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero) # MOUSEEVENTF_LEFTUP
    Write-Output "clicked (synthetic mouse at $cx,$cy): $($el.Current.Name)"
}

function Take-Screenshot {
    if (-not $Out) {
        Write-Error "-Out <path> is required for screenshot."
        exit $ExitScreenshotFailed
    }
    $hwnd = Get-ZedHwnd

    $rect = New-Object RunZed.Native+RECT
    [RunZed.Native]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    if ($width -le 0 -or $height -le 0) {
        Write-Error "Zed window has invalid dimensions ($width x $height)."
        exit $ExitScreenshotFailed
    }

    $bitmap = New-Object System.Drawing.Bitmap $width, $height
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $hdc = $graphics.GetHdc()
    try {
        # PW_RENDERFULLCONTENT: required to capture GPU/DirectX-composited content;
        # a plain BitBlt/CopyFromScreen can silently miss accelerated rendering.
        [RunZed.Native]::PrintWindow($hwnd, $hdc, 0x00000002) | Out-Null
    } finally {
        $graphics.ReleaseHdc($hdc)
    }
    $graphics.Dispose()

    # Reject a uniformly one-color capture rather than reporting false success --
    # this is exactly the failure signature the WSL/scrot approach hit.
    $isUniform = $true
    $firstPixel = $bitmap.GetPixel(0, 0)
    $sampleStepX = [Math]::Max(1, [int]($width / 20))
    $sampleStepY = [Math]::Max(1, [int]($height / 20))
    for ($x = 0; $x -lt $width -and $isUniform; $x += $sampleStepX) {
        for ($y = 0; $y -lt $height -and $isUniform; $y += $sampleStepY) {
            if ($bitmap.GetPixel($x, $y) -ne $firstPixel) {
                $isUniform = $false
            }
        }
    }

    $bitmap.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
    $bitmap.Dispose()

    if ($isUniform) {
        Write-Error "Captured screenshot is a single uniform color ($firstPixel) - capture likely failed rather than the window legitimately being blank. Saved to $Out for inspection anyway."
        exit $ExitScreenshotFailed
    }

    Write-Output "screenshot saved: $Out ($width x $height)"
}

# Keyboard input has to go to the foreground window, so every key/paste command
# activates Zed first. SetForegroundWindow alone is unreliable when the caller
# doesn't already own the foreground, hence the AppActivate fallback.
# AppActivate first, SetForegroundWindow second: Windows refuses
# SetForegroundWindow to a process that didn't receive the last input, which is
# exactly the situation an agent-driven script is in. AppActivate goes through
# the shell and isn't subject to that rule. Retried because activation can lose
# a race with a window that is still finishing its first frame.
function Activate-Zed {
    $state = Load-State
    $hwnd = Get-ZedHwnd
    [RunZed.Native]::ShowWindow($hwnd, 9) | Out-Null  # SW_RESTORE

    $shell = $null
    try { $shell = New-Object -ComObject WScript.Shell } catch {}

    for ($attempt = 0; $attempt -lt 6; $attempt++) {
        if ([RunZed.Native]::GetForegroundWindow() -eq $hwnd) {
            Write-Output "focused hwnd=$hwnd"
            return
        }
        if ($attempt -lt 2) {
            if ($null -ne $shell) {
                try { $shell.AppActivate([int]$state.Pid) | Out-Null } catch {}
            }
            [RunZed.Native]::SetForegroundWindow($hwnd) | Out-Null
        } else {
            # AppActivate can report success while Windows still refuses the
            # foreground switch (another app holds the foreground lock, e.g. the
            # browser we just drove through an OAuth screen). Minimizing and
            # restoring reactivates the window through the shell, which the lock
            # does not block.
            [RunZed.Native]::ShowWindow($hwnd, 6) | Out-Null  # SW_MINIMIZE
            Start-Sleep -Milliseconds 400
            [RunZed.Native]::ShowWindow($hwnd, 9) | Out-Null  # SW_RESTORE
        }
        Start-Sleep -Milliseconds 500
    }

    if ([RunZed.Native]::GetForegroundWindow() -ne $hwnd) {
        Write-Error "Could not bring Zed to the foreground after 5 attempts; keystrokes would land in another window."
        exit $ExitClickFailed
    }
    Write-Output "focused hwnd=$hwnd"
}

# Raw SendKeys syntax, for chords and named keys: "^+p" (ctrl+shift+p),
# "{ENTER}", "{ESC}", "{DOWN}". Literal text should go through 'paste' instead,
# since SendKeys treats +^%~(){}[] as syntax.
function Send-Keys {
    if (-not $Text) {
        Write-Error "-Text <sendkeys-sequence> is required for keys."
        exit $ExitElementNotFound
    }
    Activate-Zed | Out-Null
    [System.Windows.Forms.SendKeys]::SendWait($Text)
    Start-Sleep -Milliseconds 300
    Write-Output "sent keys: $Text"
}

# Clipboard + ctrl+V rather than per-character SendKeys: no escaping pitfalls and
# it can't drop characters in a URL.
function Paste-Text {
    if (-not $Text) {
        Write-Error "-Text <string> is required for paste."
        exit $ExitElementNotFound
    }
    Set-Clipboard -Value $Text
    Activate-Zed | Out-Null
    [System.Windows.Forms.SendKeys]::SendWait('^v')
    Start-Sleep -Milliseconds 300
    Write-Output "pasted: $Text"
}

function Quit-Zed {
    $state = Load-State
    Stop-Process -Id $state.Pid -Force -ErrorAction SilentlyContinue
    Remove-Item -Path $StateFile -ErrorAction SilentlyContinue
    Write-Output "quit pid=$($state.Pid)"
}

switch ($Command) {
    'build'          { Build-Zed }
    'launch'         { Launch-Zed }
    'list-elements'  { List-Elements }
    'click'          { Click-Element }
    'screenshot'     { Take-Screenshot }
    'focus'          { Activate-Zed }
    'keys'           { Send-Keys }
    'paste'          { Paste-Text }
    'quit'           { Quit-Zed }
}

exit $ExitOk

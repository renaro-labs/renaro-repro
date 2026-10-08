<#
.SYNOPSIS
    Adds a directory (default: the current directory) to your user PATH so `repro` works
    from any new terminal. No admin rights needed; only your user PATH is changed.

.EXAMPLE
    .\add-to-path.ps1                    # add the current directory
    .\add-to-path.ps1 C:\tools\repro     # add a specific directory
    .\add-to-path.ps1 -Remove            # remove the current directory again
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string]$Path = (Get-Location).ProviderPath,
    [switch]$Remove
)

$ErrorActionPreference = 'Stop'

# Normalize: full path, no surrounding quotes, no trailing backslash (except drive roots like C:\).
$Path = $Path.Trim().Trim('"')
if (-not [System.IO.Path]::IsPathRooted($Path)) {
    # .NET resolves relative paths against the process directory, not PowerShell's location.
    $Path = Join-Path (Get-Location).ProviderPath $Path
}
$dir = [System.IO.Path]::GetFullPath($Path).TrimEnd('\')
if ($dir -match '^[A-Za-z]:$') { $dir += '\' }

if (-not $Remove) {
    if (-not (Test-Path -LiteralPath $dir -PathType Container)) {
        Write-Error "Directory not found: $dir"
        exit 1
    }
    if (-not (Test-Path -LiteralPath (Join-Path $dir 'repro.exe'))) {
        Write-Warning "repro.exe is not in $dir (adding the directory anyway)."
    }
}

function Test-SameDir([string]$entry, [string]$target) {
    $expanded = [Environment]::ExpandEnvironmentVariables($entry.Trim().Trim('"')).TrimEnd('\')
    return $expanded -ieq $target.TrimEnd('\')
}

# Read the user PATH unexpanded, so entries like %USERPROFILE%\bin stay as they are.
$key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
try {
    $raw = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $entries = @($raw -split ';' | Where-Object { $_.Trim() -ne '' })
    $present = @($entries | Where-Object { Test-SameDir $_ $dir }).Count -gt 0

    if ($Remove) {
        if (-not $present) {
            Write-Host "Not in your user PATH: $dir"
            exit 0
        }
        $entries = @($entries | Where-Object { -not (Test-SameDir $_ $dir) })
        $message = "Removed from your user PATH: $dir"
    } else {
        if ($present) {
            Write-Host "Already in your user PATH: $dir"
            exit 0
        }
        $entries += $dir
        $message = "Added to your user PATH: $dir"
    }

    # ExpandString keeps %VAR% entries working (setx would also truncate at 1024 chars).
    $key.SetValue('Path', ($entries -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
} finally {
    $key.Close()
}

# Tell Explorer and other programs that the environment changed, so new terminals see it.
try {
    if (-not ('Repro.PathNotify' -as [type])) {
        Add-Type -Namespace 'Repro' -Name 'PathNotify' -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true, CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern System.IntPtr SendMessageTimeout(System.IntPtr hWnd, uint Msg, System.UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out System.UIntPtr lpdwResult);
'@
    }
    $result = [UIntPtr]::Zero
    [void][Repro.PathNotify]::SendMessageTimeout([IntPtr]0xffff, 0x001A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result)
} catch {
    Write-Verbose "Could not broadcast the environment change: $_"
}

# Also update this PowerShell session.
$sessionEntries = @($env:Path -split ';' | Where-Object { $_ -ne '' -and -not (Test-SameDir $_ $dir) })
if (-not $Remove) { $sessionEntries += $dir }
$env:Path = $sessionEntries -join ';'

Write-Host $message
Write-Host 'Open a new terminal (or restart your editor) for the change to take effect everywhere.'

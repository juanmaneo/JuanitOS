# Run a shell command inside WSL as a detached session that survives the
# calling terminal (WSL2 stops its VM when no session is open, so a plain
# `nohup ... &` inside WSL dies after the idle timeout).
#
#   tools/wsl-bg.ps1 <log-name> <command...>
#
# Output goes to $JOS_WORK/logs/<log-name>.log inside WSL (JOS_WORK
# defaults to ~/juanitos-work). The command runs from the repository root
# with cargo on PATH and JOS_WORK exported.
param(
    [Parameter(Mandatory = $true)][string]$LogName,
    [Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)][string[]]$Command
)

function ToWsl([string]$p) {
    $full = (Resolve-Path $p).Path
    return '/mnt/' + $full.Substring(0, 1).ToLower() + $full.Substring(2).Replace('\', '/')
}

$repo = ToWsl (Join-Path $PSScriptRoot '..')
$cmd = ($Command -join ' ')
# The command goes through a script file: quoting across PowerShell ->
# wsl.exe -> bash is otherwise fragile.
$script = Join-Path $env:TEMP "jos-bg-$LogName.sh"
$body = @"
export JOS_WORK="`${JOS_WORK:-`$HOME/juanitos-work}"
export PATH="`$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin"
mkdir -p "`$JOS_WORK/logs"
cd '$repo' || exit 1
{ $cmd; echo "EXIT=`$?"; } > "`$JOS_WORK/logs/$LogName.log" 2>&1
"@
[System.IO.File]::WriteAllText($script, $body.Replace("`r`n", "`n"))
$wslScript = ToWsl $script
Start-Process -FilePath 'wsl.exe' -ArgumentList @('-e', 'bash', $wslScript) -WindowStyle Hidden
Write-Output "started: $LogName (log: `$JOS_WORK/logs/$LogName.log)"

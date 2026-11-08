# Creates and attaches an empty virtual disk with one volume on it, for tests
# that need a particular file system or a volume that can safely lose its journal.
# Used by the CI workflow, and needs an elevated process.
param(
    [Parameter(Mandatory)] [ValidateSet('NTFS', 'ReFS')] [string] $FileSystem,
    [Parameter(Mandatory)] [ValidatePattern('^[A-Z]$')] [string] $Letter,
    [int] $SizeMb = 4096
)
$ErrorActionPreference = 'Stop'

$directory = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { $env:TEMP }
$disk = Join-Path $directory "usn-test-$Letter.vhdx"
$commands = Join-Path $directory "usn-test-$Letter.diskpart"

@"
create vdisk file="$disk" maximum=$SizeMb type=expandable
select vdisk file="$disk"
attach vdisk
create partition primary
format fs=$FileSystem quick label=USNTEST
assign letter=$Letter
"@ | Set-Content -Encoding Ascii -LiteralPath $commands

diskpart /s $commands
if ($LASTEXITCODE -ne 0) { throw "diskpart failed with exit code $LASTEXITCODE" }

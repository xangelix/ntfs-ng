#Requires -RunAsAdministrator
<#
.SYNOPSIS
Prepares an NTFS volume for tests/live_windows.rs.

.DESCRIPTION
Creates a VHDX, attaches it, formats it NTFS with 1 KiB or 4 KiB file records, and fills
fixtures\ with a file of every kind the reader must handle: empty, resident and not, Unicode
names, a path past MAX_PATH, compressed (LZNT1), sparse, uninitialized past its valid data,
fragmented past its record (runs in extension records), alternate data streams, hard links (300
of one file, past its record), symbolic links and a junction, and deleted files. The volume is
flushed, so a raw read sees what the files hold.

With -Shadow, it then takes a shadow copy and changes fixtures\snapshot after it.

In GitHub Actions it sets NTFS_NG_LIVE_VOLUME, NTFS_NG_LIVE_RECORD_SIZE and, with -Shadow,
NTFS_NG_LIVE_SHADOW for the steps after it; elsewhere it prints them.

.EXAMPLE
./scripts/windows-live-fixtures.ps1 -Letter N -RecordSize 4096 -Shadow
cargo test --all-features --release --test live_windows -- --ignored
#>
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '',
    Justification = 'Its own helpers, on a volume it made')]
param(
    [ValidatePattern('^[D-Z]$')] [string] $Letter = 'N',
    [ValidateSet(1024, 4096)] [int] $RecordSize = 1024,
    [string] $Vhd = (Join-Path ([IO.Path]::GetTempPath()) 'ntfs-ng-live.vhdx'),
    [int] $SizeMiB = 1024,
    [switch] $Shadow
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-Native([string] $File, [string[]] $Arguments) {
    $output = & $File @Arguments
    if ($LASTEXITCODE -ne 0) {
        $output | Out-Host
        throw "$File $Arguments failed with $LASTEXITCODE"
    }
    $output
}

function Set-Env([string] $Name, [string] $Value) {
    if ($env:GITHUB_ENV) {
        "$Name=$Value" | Add-Content -Encoding utf8 $env:GITHUB_ENV
    }
    Set-Item "env:$Name" $Value
    Write-Output "$Name=$Value"
}

# Paths are written through \\?\, so that none is limited to MAX_PATH.
function Get-Long([string] $Path) { "\\?\$Path" }

function New-Dir([string] $Path) {
    $null = [IO.Directory]::CreateDirectory((Get-Long $Path))
    $Path
}

function Write-Fixture([string] $Path, [byte[]] $Bytes) {
    [IO.File]::WriteAllBytes((Get-Long $Path), $Bytes)
}

function Add-Fixture([string] $Path, [byte[]] $Bytes) {
    $stream = [IO.File]::Open((Get-Long $Path), [IO.FileMode]::Append)
    try { $stream.Write($Bytes, 0, $Bytes.Length) } finally { $stream.Dispose() }
}

function Get-Noise([int] $Length, [int] $Seed) {
    $bytes = [byte[]]::new($Length)
    [Random]::new($Seed).NextBytes($bytes)
    , $bytes
}

function Get-Text([int] $Length) {
    $line = 'NTFS compresses text like this in units of sixteen clusters. '
    $text = $line * [Math]::Ceiling($Length / $line.Length)
    , [Text.Encoding]::ASCII.GetBytes($text.Substring(0, $Length))
}

function Get-Ascii([string] $Text) { , [Text.Encoding]::ASCII.GetBytes($Text) }

# The volume.
if (Test-Path $Vhd) { Remove-Item $Vhd }
$script = Join-Path ([IO.Path]::GetTempPath()) 'ntfs-ng-diskpart.txt'
"create vdisk file=`"$Vhd`" maximum=$SizeMiB type=expandable" | Set-Content -Encoding ascii $script
$null = Invoke-Native diskpart.exe @('/s', $script)
$null = Mount-DiskImage -ImagePath $Vhd
$disk = Get-DiskImage -ImagePath $Vhd | Get-Disk
Initialize-Disk -Number $disk.Number -PartitionStyle GPT
$partition = New-Partition -DiskNumber $disk.Number -UseMaximumSize -DriveLetter $Letter
$null = Format-Volume -Partition $partition -FileSystem NTFS -NewFileSystemLabel 'ntfs-ng' `
    -UseLargeFRS:($RecordSize -eq 4096) -Force -Confirm:$false
$info = Invoke-Native fsutil.exe @('fsinfo', 'ntfsinfo', "${Letter}:")
$info | Out-Host
if (-not ($info -match "Bytes Per FileRecord Segment\s*:\s*$RecordSize\b")) {
    throw "the volume was not formatted with $RecordSize-byte file records"
}

$root = New-Dir "${Letter}:\fixtures"

# Plain files, resident and not.
Write-Fixture "$root\empty.bin" ([byte[]]::new(0))
Write-Fixture "$root\tiny.txt" (Get-Ascii 'resident: held in the record itself')
Write-Fixture "$root\small.bin" (Get-Noise 3000 1)
Write-Fixture "$root\medium.bin" (Get-Noise 1MB 2)
$unicode = "unicode-$([char]0x65E5)$([char]0x672C)$([char]0x8A9E)-$([char]0x00F1)and$([char]0x00FA).txt"
Write-Fixture "$root\$unicode" ([Text.Encoding]::UTF8.GetBytes("$([char]0x540D)$([char]0x524D)"))

# A path well past MAX_PATH.
$deep = $root
foreach ($level in 1..12) {
    $deep = New-Dir (Join-Path $deep "level-$level-$('x' * 24)")
}
Write-Fixture "$deep\deep.txt" (Get-Noise 2000 3)

# Compressed: text, noise (stored in its compression units as is), and the two mixed.
$compressed = New-Dir "$root\compressed"
Write-Fixture "$compressed\text.txt" (Get-Text 8MB)
Write-Fixture "$compressed\noise.bin" (Get-Noise 1MB 4)
Write-Fixture "$compressed\mixed.bin" (Get-Text 256KB)
Add-Fixture "$compressed\mixed.bin" (Get-Noise 256KB 5)
Add-Fixture "$compressed\mixed.bin" (Get-Text 300000)
$null = Invoke-Native compact.exe @('/c', "/s:$compressed", '/q')

# Sparse: a range deallocated in the middle, and the end never written.
$sparse = New-Dir "$root\sparse"
Write-Fixture "$sparse\holes.bin" (Get-Noise 16MB 6)
$null = Invoke-Native fsutil.exe @('sparse', 'setflag', "$sparse\holes.bin")
$null = Invoke-Native fsutil.exe @('sparse', 'setrange', "$sparse\holes.bin", "$(4MB)", "$(8MB)")
$null = Invoke-Native fsutil.exe @('file', 'seteof', "$sparse\holes.bin", "$(64MB)")

# Allocated but never written: read as zeros past the valid data length.
$null = Invoke-Native fsutil.exe @('file', 'createnew', "$root\uninitialized.bin", "$(1MB)")

# Alternate data streams, resident and not.
$streams = New-Dir "$root\streams"
Write-Fixture "$streams\ads.txt" (Get-Ascii 'the unnamed stream')
Write-Fixture "$streams\ads.txt:note" (Get-Ascii 'a note beside it')
Write-Fixture "$streams\ads.txt:big" (Get-Noise 1MB 7)

# Hard links: three names in one directory and one in another; and 300 names of one file, more
# than its record holds.
$links = New-Dir "$root\links"
Write-Fixture "$links\target.txt" (Get-Noise 5000 8)
$other = New-Dir "$links\other"
foreach ($link in "$links\second.txt", "$links\third.txt", "$other\elsewhere.txt") {
    $null = New-Item -ItemType HardLink -Path $link -Target "$links\target.txt"
}
$many = New-Dir "$links\many"
Write-Fixture "$many\many.bin" (Get-Noise 10000 9)
foreach ($at in 1..300) {
    $null = New-Item -ItemType HardLink -Path ("$many\link-{0:D3}.bin" -f $at) -Target "$many\many.bin"
}

# Reparse points.
$reparse = New-Dir "$root\reparse"
$null = New-Item -ItemType SymbolicLink -Path "$reparse\file-link" -Target "$root\tiny.txt"
$null = New-Item -ItemType SymbolicLink -Path "$reparse\dir-link" -Target $links
$null = New-Item -ItemType Junction -Path "$reparse\junction" -Target $streams

# What a shadow copy will hold, and what will change after it.
$snapshot = New-Dir "$root\snapshot"
Write-Fixture "$snapshot\changed.txt" (Get-Ascii 'before the snapshot')
Write-Fixture "$snapshot\removed.txt" (Get-Ascii 'deleted after the snapshot')

# Fragmented: the volume filled with 64 KiB files, every other one deleted, and a file written
# into the holes, in more extents than its record holds.
$filler = New-Dir "${Letter}:\filler"
$block = Get-Noise 64KB 10
$count = 0
$full = $false
try {
    while ($true) {
        [IO.File]::WriteAllBytes("$filler\$count.bin", $block)
        $count++
    }
} catch [IO.IOException] {
    $full = $true
}
Write-Output "the volume was full after $count filler files: $full"
for ($at = 0; $at -lt $count; $at += 2) {
    [IO.File]::Delete("$filler\$at.bin")
}
$fragmented = New-Dir "$root\fragmented"
$pieces = [int][Math]::Min(96, [Math]::Floor($count / 2 * 64KB * 0.6 / 1MB))
if ($pieces -lt 32) {
    throw "only $count filler files fit: too little room to fragment a file"
}
foreach ($at in 1..$pieces) {
    Add-Fixture "$fragmented\big.bin" (Get-Noise 1MB (1000 + $at))
}
Remove-Item -Recurse -Force $filler
$extents = @(Invoke-Native fsutil.exe @('file', 'queryextents', "$fragmented\big.bin") |
        Where-Object { $_ -match 'VCN' }).Count
Write-Output "fragmented\big.bin: $pieces MiB in $extents extents"
# About five bytes of mapping pairs each: past what a 4 KiB record holds.
if ($extents -lt 1200) {
    throw "big.bin is in $extents extents, too few to need an attribute list"
}

# Deleted files, last: a file created after them could take their records (the filler files
# above took every one). Windows may still create a few itself (a shadow copy's storage), so
# there are many.
$deleted = New-Dir "$root\deleted"
foreach ($at in 1..200) {
    Write-Fixture ("$deleted\gone-{0:D3}.bin" -f $at) (Get-Noise 8192 (100 + $at))
}
Get-ChildItem $deleted | Remove-Item

Write-VolumeCache -DriveLetter $Letter
Set-Env NTFS_NG_LIVE_VOLUME "${Letter}:"
Set-Env NTFS_NG_LIVE_RECORD_SIZE $RecordSize

if ($Shadow) {
    $created = Invoke-CimMethod -ClassName Win32_ShadowCopy -MethodName Create `
        -Arguments @{ Volume = "${Letter}:\"; Context = 'ClientAccessible' }
    if ($created.ReturnValue -ne 0) {
        throw "Win32_ShadowCopy.Create failed with $($created.ReturnValue)"
    }
    $copy = Get-CimInstance Win32_ShadowCopy | Where-Object ID -EQ $created.ShadowID
    Write-Fixture "$snapshot\changed.txt" (Get-Ascii 'after the snapshot')
    Remove-Item "$snapshot\removed.txt"
    Write-VolumeCache -DriveLetter $Letter
    Set-Env NTFS_NG_LIVE_SHADOW $copy.DeviceObject
}

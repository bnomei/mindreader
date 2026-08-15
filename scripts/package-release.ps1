param(
  [Parameter(Mandatory = $true)][string]$Target,
  [Parameter(Mandatory = $true)][string]$Version,
  [string]$BinName = 'mindreader',
  [string]$OutDir = 'dist'
)

$ErrorActionPreference = 'Stop'
$metadata = cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
$targetDir = $metadata.target_directory
$binPath = Join-Path $targetDir "$Target/release/$BinName.exe"
if (-not (Test-Path $binPath)) { throw "Binary not found: $binPath" }

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$outDirFull = (Resolve-Path $OutDir).Path
$archiveName = "$BinName-v$Version-$Target.zip"
$archivePath = Join-Path $outDirFull $archiveName
$tempDir = Join-Path $env:TEMP ("mindreader-package-" + [Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Force -Path $tempDir | Out-Null
try {
  Copy-Item -Path $binPath -Destination (Join-Path $tempDir "$BinName.exe") -Force
  Copy-Item -Path 'LICENSE' -Destination (Join-Path $tempDir 'LICENSE') -Force
  Compress-Archive -Path (Join-Path $tempDir "$BinName.exe") -DestinationPath $archivePath -Force
  Compress-Archive -Path (Join-Path $tempDir 'LICENSE') -Update -DestinationPath $archivePath
} finally {
  Remove-Item -Recurse -Force $tempDir
}

$hash = Get-FileHash -Algorithm SHA256 -Path $archivePath
"$($hash.Hash)  $archiveName" | Out-File -FilePath "$archivePath.sha256" -Encoding ascii

param(
  [Parameter(Mandatory = $true)][string]$Target,
  [string]$PackageName = 'mindreader'
)

$ErrorActionPreference = 'Stop'
cargo build --locked --release -p $PackageName --bin $PackageName --target $Target

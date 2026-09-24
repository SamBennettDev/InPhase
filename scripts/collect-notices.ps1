<# .SYNOPSIS Collect license texts from the exact dependency sources used by this build. #>
[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$OutDir)
$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $true
$root = Split-Path -Parent $PSScriptRoot
$licensePattern = '^(license|licence|copying|notice|copyright)([._-]|$)'
$inventory = @()
Push-Location $root
try {
    $json = & cargo metadata --locked --format-version 1 --filter-platform x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw "Cannot collect Cargo dependency metadata." }
    $metadata = $json | ConvertFrom-Json
    foreach ($package in $metadata.packages) {
        $source = Split-Path -Parent $package.manifest_path
        $destination = Join-Path $OutDir "rust\$($package.name)-$($package.version)"
        $files = @(Get-ChildItem $source -File | Where-Object { $_.Name -match $licensePattern })
        if ($package.license_file) {
            $declared = Join-Path $source $package.license_file
            if (Test-Path $declared) { $files += Get-Item $declared }
        }
        if ($files.Count) {
            New-Item -ItemType Directory -Force $destination | Out-Null
            $files | Sort-Object FullName -Unique | ForEach-Object { Copy-Item $_.FullName $destination }
        }
        $inventory += [pscustomobject]@{
            name = $package.name
            version = $package.version
            license = $package.license
            repository = $package.repository
            notice_files = @($files | ForEach-Object { $_.Name } | Sort-Object -Unique)
        }
    }
    # The metadata graph includes build and development dependencies as well.
    $inventory | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $OutDir "rust-dependencies.json") -Encoding utf8
    $web = Get-Content (Join-Path $root "web\package.json") -Raw | ConvertFrom-Json
    foreach ($name in $web.dependencies.PSObject.Properties.Name) {
        $source = Join-Path $root "web\node_modules\$name"
        $files = @(Get-ChildItem $source -File | Where-Object { $_.Name -match $licensePattern })
        if (-not $files.Count) { throw "Missing license notice for web runtime dependency: $name" }
        $destination = Join-Path $OutDir ("web\" + ($name -replace '[/@]', '_'))
        New-Item -ItemType Directory -Force $destination | Out-Null
        $files | ForEach-Object { Copy-Item $_.FullName $destination }
        Copy-Item (Join-Path $source "package.json") $destination
    }
} finally { Pop-Location }

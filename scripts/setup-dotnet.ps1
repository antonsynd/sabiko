<#
.SYNOPSIS
  Windows counterpart of setup-dotnet.sh: fetches ilasm, ildasm, ILVerify and the
  net10.0 reference assemblies from NuGet into .\tools, reusing an existing `dotnet`
  (install the .NET 10 runtime first, e.g. with actions/setup-dotnet or winget).
  Writes tools\env.ps1; dot-source it afterwards.
#>
param([switch]$Force)
$ErrorActionPreference = 'Stop'
$Root = Resolve-Path (Join-Path $PSScriptRoot '..')
$Tools = Join-Path $Root 'tools'; $Cache = Join-Path $Tools '.cache'; $Bin = Join-Path $Tools 'bin'
$Version = if ($env:SABIKO_DOTNET_VERSION) { $env:SABIKO_DOTNET_VERSION } else { '10.0.13' }
$Rid = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq 'Arm64') { 'win-arm64' } else { 'win-x64' }
New-Item -ItemType Directory -Force -Path $Bin, $Cache | Out-Null

$dotnet = Get-Command dotnet -ErrorAction SilentlyContinue
if (-not $dotnet) { throw 'dotnet not found on PATH; install the .NET 10 runtime first' }
if (-not ((& $dotnet.Source --list-runtimes) -match '^Microsoft.NETCore.App 10\.')) { throw 'a .NET 10 runtime is required' }

function Fetch($Id, $Ver) {
  $out = Join-Path $Cache "$Id.zip"
  if ($Force -or -not (Test-Path $out)) {
    Write-Host "[setup-dotnet] downloading $Id $Ver"
    Invoke-WebRequest -Uri "https://api.nuget.org/v3-flatcontainer/$Id/$Ver/$Id.$Ver.nupkg" -OutFile $out
  }
  $out
}
function Extract($Zip, $Prefix, $Dest) {
  New-Item -ItemType Directory -Force -Path $Dest | Out-Null
  Add-Type -AssemblyName System.IO.Compression.FileSystem
  $z = [System.IO.Compression.ZipFile]::OpenRead($Zip)
  try {
    foreach ($e in $z.Entries) {
      if ($e.FullName.StartsWith($Prefix) -and $e.Name) {
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile($e, (Join-Path $Dest $e.Name), $true)
      }
    }
  } finally { $z.Dispose() }
}

foreach ($t in 'ilasm', 'ildasm') { Extract (Fetch "runtime.$Rid.microsoft.netcore.$t" $Version) "runtimes/$Rid/native/" $Bin }
Remove-Item -Recurse -Force (Join-Path $Tools 'ilverify') -ErrorAction SilentlyContinue
Extract (Fetch 'dotnet-ilverify' $Version) 'tools/net10.0/any/' (Join-Path $Tools 'ilverify')
Remove-Item -Recurse -Force (Join-Path $Tools 'ref') -ErrorAction SilentlyContinue
Extract (Fetch 'microsoft.netcore.app.ref' $Version) 'ref/net10.0/' (Join-Path $Tools 'ref')
Get-ChildItem (Join-Path $Tools 'ref') -Exclude *.dll | Remove-Item -Force

@"
@echo off
"$($dotnet.Source)" "$Tools\ilverify\ILVerify.dll" -r "$Tools\ref\*.dll" %*
"@ | Set-Content (Join-Path $Bin 'ilverify.cmd')

@"
`$env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'
`$env:DOTNET_NOLOGO = '1'
`$env:SABIKO_DOTNET = '$($dotnet.Source)'
`$env:SABIKO_REF_DIR = '$Tools\ref'
`$env:PATH = '$Bin;' + `$env:PATH
"@ | Set-Content (Join-Path $Tools 'env.ps1')
Write-Host '[setup-dotnet] done. Run: . tools\env.ps1'

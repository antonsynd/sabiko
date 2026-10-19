<#
.SYNOPSIS
  Windows counterpart of build-runtime.sh: compiles runtime\Sabiko.Runtime\*.cs into
  tools\runtime\Sabiko.Runtime.dll with the Roslyn csc installed by setup-dotnet.ps1
  (no .NET SDK needed) and verifies the result with ILVerify. Re-run after editing the C#.
#>
$ErrorActionPreference = 'Stop'
$Root = Resolve-Path (Join-Path $PSScriptRoot '..')
$Tools = Join-Path $Root 'tools'
$Src = Join-Path $Root 'runtime\Sabiko.Runtime'
$Out = Join-Path $Tools 'runtime'
$Csc = Join-Path $Tools 'bin\csc.cmd'
$IlVerify = Join-Path $Tools 'bin\ilverify.cmd'
if (-not ((Test-Path $Csc) -and (Test-Path (Join-Path $Tools 'ref')))) { throw 'toolchain missing; run scripts\setup-dotnet.ps1 first' }

New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Dll = Join-Path $Out 'Sabiko.Runtime.dll'
# Response file: one reference per line for every net10.0 reference assembly, then the sources.
$Rsp = Join-Path $Out 'Sabiko.Runtime.rsp'
$lines = @()
foreach ($r in Get-ChildItem (Join-Path $Tools 'ref') -Filter *.dll) { $lines += "-r:`"$($r.FullName)`"" }
foreach ($cs in Get-ChildItem $Src -Filter *.cs) { $lines += "`"$($cs.FullName)`"" }
Set-Content -Path $Rsp -Value $lines

Write-Host "[build-runtime] compiling $Src -> $Dll"
& $Csc -nologo -noconfig -nostdlib -target:library -optimize+ -deterministic `
  -nullable:enable -langversion:latest -warnaserror `
  "-out:$Dll" "-doc:$(Join-Path $Out 'Sabiko.Runtime.xml')" "@$Rsp"
if ($LASTEXITCODE -ne 0) { throw "csc failed with exit code $LASTEXITCODE" }

Write-Host '[build-runtime] verifying'
$log = Join-Path $Out 'ilverify.log'
& $IlVerify $Dll | Set-Content $log
if ($LASTEXITCODE -ne 0) { Get-Content $log | Write-Host; throw 'build-runtime: Sabiko.Runtime.dll does not verify' }
Write-Host '[build-runtime] ilverify: ok'

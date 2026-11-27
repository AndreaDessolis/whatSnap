# Fails if a shipped exe needs the Visual C++ redistributable (vcruntime140, msvcp140, ...)
# or if a DLL got into the package. Windows' own Universal CRT (api-ms-win-crt-*, ucrtbase)
# is allowed: it is part of every Windows 10 and 11. Run from the repository root after
# `cargo release`, in a shell where dumpbin is on PATH (Visual Studio's developer prompt).
$ErrorActionPreference = 'Stop'
$exes = 'whatSnap.exe', 'whatSnapShare.exe', 'backend\backend.exe' | ForEach-Object { Join-Path target\release $_ }
$redist = '^(vcruntime\d+|msvcp\d+|msvcr\d+|concrt\d+|vccorlib\d+|vcomp\d+|ucrtbased)(_\w+)?\.dll$'
$failed = $false
foreach ($exe in $exes) {
	if (-not (Test-Path $exe)) { throw "$exe is missing" }
	$dlls = dumpbin /nologo /dependents $exe | ForEach-Object { $_.Trim() } | Where-Object { $_ -match '\.dll$' }
	if ($LASTEXITCODE -ne 0) { throw "dumpbin failed on $exe" }
	Write-Output "$exe imports: $($dlls -join ', ')"
	$bad = $dlls | Where-Object { $_ -match $redist }
	if ($bad) {
		Write-Output "::error::$exe needs the Visual C++ redistributable: $($bad -join ', ')"
		$failed = $true
	}
}
Add-Type -AssemblyName System.IO.Compression.FileSystem
foreach ($zip in Get-ChildItem target\release -Filter 'whatSnap*.zip') {
	$archive = [System.IO.Compression.ZipFile]::OpenRead($zip.FullName)
	try {
		$shipped = $archive.Entries | Where-Object { $_.FullName -match '\.dll$' } | ForEach-Object FullName
	} finally {
		$archive.Dispose()
	}
	if ($shipped) {
		Write-Output "::error::$($zip.Name) ships DLLs: $($shipped -join ', ')"
		$failed = $true
	}
}
if ($failed) { exit 1 }

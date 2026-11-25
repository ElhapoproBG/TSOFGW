# Sube TSOFGW a git. Pensado para ejecutarse cada medianoche con el Programador de tareas.
$ErrorActionPreference = "Stop"
Set-Location "C:\TSOFGW"
if (-not (Test-Path ".git")) { git init -b main | Out-Null }
git add -A
git diff --cached --quiet
if ($LASTEXITCODE -eq 0) { Write-Output "Sin cambios $(Get-Date -Format s)"; exit 0 }
git commit -m "Respaldo automatico $(Get-Date -Format 'yyyy-MM-dd HH:mm')"
$remote = git remote
if (-not $remote) { Write-Output "Commit local hecho. Falta configurar remoto: git remote add origin <URL>"; exit 0 }
git push origin HEAD

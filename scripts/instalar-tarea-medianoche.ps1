# Ejecuta UNA vez (PowerShell normal) para programar la subida diaria a las 00:00.
$accion = New-ScheduledTaskAction -Execute "powershell.exe" -Argument '-NoProfile -ExecutionPolicy Bypass -File "C:\TSOFGW\scripts\subir-a-git.ps1"'
$disparador = New-ScheduledTaskTrigger -Daily -At 12:00AM
$config = New-ScheduledTaskSettingsSet -StartWhenAvailable
Register-ScheduledTask -TaskName "TSOFGW-subir-a-git" -Action $accion -Trigger $disparador -Settings $config -Description "Sube TSOFGW a git cada medianoche"

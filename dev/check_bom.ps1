$f="d:\projects\rust\agent-study\apps\rollball-desktop\src\components\results\AgentSetupTab.tsx"; $b=[IO.File]::ReadAllBytes($f); Write-Host ($b[0..15] | ForEach-Object { $_.ToString("X2") })

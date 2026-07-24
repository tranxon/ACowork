Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'" |
  Where-Object { $_.CommandLine -match 'acowork-desktop' } |
  Select-Object ProcessId,
    @{N='Type';E={ if ($_.CommandLine -match '--type=(\S+)') { $matches[1] } else { 'browser' } }},
    @{N='WS_MB';E={ [math]::Round((Get-Process -Id $_.ProcessId).WorkingSet64 / 1MB, 1) }} |
  Format-Table -AutoSize
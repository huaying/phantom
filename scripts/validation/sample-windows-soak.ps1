param([int]$DurationSeconds=4500,[int]$IntervalSeconds=30)
$ErrorActionPreference='Stop';$ProgressPreference='SilentlyContinue';[Console]::OutputEncoding=[Text.Encoding]::UTF8
$until=[DateTime]::UtcNow.AddSeconds($DurationSeconds)
do {
 $os=Get-CimInstance Win32_OperatingSystem
 [ordered]@{
  utc=[DateTime]::UtcNow.ToString('o');boot=$os.LastBootUpTime.ToUniversalTime().ToString('o')
  freePhysicalKB=$os.FreePhysicalMemory
  phantom=@(Get-Process phantom-server -ErrorAction SilentlyContinue | ForEach-Object { [ordered]@{id=$_.Id;cpuSeconds=$_.CPU;workingSetBytes=$_.WorkingSet64;privateBytes=$_.PrivateMemorySize64;handles=$_.HandleCount;threads=$_.Threads.Count} })
  source=@(Get-CimInstance Win32_Process -Filter "Name='msedge.exe'" | Where-Object {$_.CommandLine -like '*phantom-media-soak-20260915-edge*'} | Select-Object ProcessId,ParentProcessId)
  gpu=(& nvidia-smi --query-gpu=utilization.gpu,memory.used --format=csv,noheader,nounits | Out-String).Trim()
 }|ConvertTo-Json -Depth 5 -Compress
 Start-Sleep -Seconds $IntervalSeconds
}while([DateTime]::UtcNow -lt $until)

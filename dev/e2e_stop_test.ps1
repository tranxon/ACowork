<#
.SYNOPSIS
  E2E test for stop response latency after control/data flow separation.
.DESCRIPTION
  1. Connects WebSocket to Gateway stream
  2. Sends a long message via HTTP API to trigger streaming
  3. On first delta, sends stop via WebSocket
  4. Measures: stop_sent -> stopped_received latency
#>
param(
    [string]$GatewayUrl = "http://127.0.0.1:19885",
    [string]$AgentId = "com.acowork.system"
)

$ErrorActionPreference = "Stop"

# --- Step 1: Connect WebSocket ---
$wsUrl = $GatewayUrl -replace "^http", "ws"
$wsUri = "$wsUrl/api/agents/$AgentId/stream"

Write-Host "[1] Connecting WebSocket to $wsUri ..."
$ws = [System.Net.WebSockets.ClientWebSocket]::new()
$cts = [System.Threading.CancellationTokenSource]::new()
$cts.CancelAfter([TimeSpan]::FromSeconds(60))

$connectTask = $ws.ConnectAsync([Uri]$wsUri, $cts.Token)
while (-not $connectTask.IsCompleted) { Start-Sleep -Milliseconds 50 }
if ($connectTask.IsFaulted) { throw "WebSocket connect failed: $($connectTask.Exception.Message)" }
Write-Host "[1] WebSocket connected. State: $($ws.State)"

# Buffer for receiving (ArraySegment for unambiguous overload)
$recvBuffer = [ArraySegment[byte]]::new([byte[]]::new(65536))

# --- Step 1b: Get session_id BEFORE model_switch ---
Write-Host "[1b] Fetching conversations ..."
$convs = Invoke-RestMethod -Uri "$GatewayUrl/api/agents/$AgentId/conversations" -Method Get -TimeoutSec 5
$sessionId = $convs.conversations[0].session_id
Write-Host "[1b] Using session_id: $sessionId"

# --- Step 1c: Send model_switch via WebSocket (with session_id) ---
$switchMsg = @{
    type = "model_switch"
    model = "MiniMax-M2.5"
    provider = "minimax-cn-coding-plan"
    session_id = $sessionId
} | ConvertTo-Json -Compress
$switchBytes = [System.Text.Encoding]::UTF8.GetBytes($switchMsg)
$switchSeg = [ArraySegment[byte]]::new($switchBytes)
$switchTask = $ws.SendAsync($switchSeg, [System.Net.WebSockets.WebSocketMessageType]::Text, $true, $cts.Token)
while (-not $switchTask.IsCompleted) { Start-Sleep -Milliseconds 10 }
Write-Host "[1c] model_switch sent (MiniMax-M2.5, session=$sessionId)"
Start-Sleep -Milliseconds 500

# --- Step 2: Send message via HTTP API ---
$messageBody = @{
    content = "Please write a very long essay about the history of computing, from the abacus to modern AI. Include at least 10 paragraphs with detailed explanations of each era."
    session_id = $sessionId
} | ConvertTo-Json

Write-Host "[2] Sending message via HTTP API ..."
$sendTime = Get-Date
$httpResp = Invoke-RestMethod -Uri "$GatewayUrl/api/agents/$AgentId/message" -Method Post -Body $messageBody -ContentType "application/json" -TimeoutSec 10
Write-Host "[2] Message sent. ID: $($httpResp.message_id), Status: $($httpResp.status)"

# --- Step 3: Receive deltas, then send stop ---
$firstDeltaTime = $null
$stopSentTime = $null
$stoppedTime = $null
$deltaCount = 0
$stopSent = $false
$done = $false
$eventCount = 0

# Send stop after receiving first few deltas
function Send-Stop($websocket, $token, $sessionId) {
    $stopMsg = @{
        type = "stop"
        session_id = $sessionId
    } | ConvertTo-Json -Compress
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($stopMsg)
    $seg = [ArraySegment[byte]]::new($bytes)
    $sendTask = $websocket.SendAsync($seg, [System.Net.WebSockets.WebSocketMessageType]::Text, $true, $token)
    while (-not $sendTask.IsCompleted) { Start-Sleep -Milliseconds 10 }
}

Write-Host "[3] Waiting for streaming output..."

while (-not $done -and $ws.State -eq 'Open') {
    $recvTask = $ws.ReceiveAsync([ArraySegment[byte]]$recvBuffer, $cts.Token)
    while (-not $recvTask.IsCompleted) { Start-Sleep -Milliseconds 10 }

    if ($recvTask.IsFaulted) {
        Write-Host "Receive error: $($recvTask.Exception.Message)"
        break
    }

    $msg = [System.Text.Encoding]::UTF8.GetString($recvBuffer.Array, 0, $recvTask.Result.Count)
    $eventCount++

    # Parse JSON
    try {
        $json = $msg | ConvertFrom-Json
    } catch {
        Write-Host "  [ws] Non-JSON (event #$eventCount): $($msg.Substring(0, [Math]::Min(200, $msg.Length)))"
        continue
    }

    # Log every event type (truncated content for debugging)
    $typeStr = $json.type
    if (-not $typeStr) { $typeStr = "(empty type)" }

    switch ($json.type) {
        "connected" {
            Write-Host "  [ws] event #$eventCount : connected (agent=$($json.agent_id))"
        }
        "chunk" {
            $deltaCount++
            if (-not $firstDeltaTime) {
                $firstDeltaTime = Get-Date
                Write-Host "  [ws] event #$eventCount : FIRST CHUNK received!"
                # Send stop immediately after first chunk
                if (-not $stopSent) {
                    $stopSentTime = Get-Date
                    Send-Stop $ws $cts.Token $sessionId
                    $stopSent = $true
                    Write-Host "  [ws] *** STOP sent! *** (after $($deltaCount) chunks, $($stopSentTime.ToString('HH:mm:ss.fff')))"
                }
            }
            # Print every 50th chunk
            if ($deltaCount % 50 -eq 0) {
                Write-Host "  [ws] chunk #$deltaCount ..."
            }
        }
        "stop_received" {
            Write-Host "  [ws] event #$eventCount : STOP_RECEIVED (Gateway ack)"
        }
        "stopped" {
            $stoppedTime = Get-Date
            Write-Host "  [ws] event #$eventCount : STOPPED received! ($($stoppedTime.ToString('HH:mm:ss.fff')))"
            $done = $true
        }
        "done" {
            Write-Host "  [ws] event #$eventCount : DONE"
            if (-not $stoppedTime) {
                $stoppedTime = Get-Date
            }
            $done = $true
        }
        "error" {
            Write-Host "  [ws] event #$eventCount : ERROR: $($json.message)"
            $done = $true
        }
        default {
            # Log all non-delta events with content preview
            $preview = $msg
            if ($preview.Length -gt 200) { $preview = $preview.Substring(0, 200) + "..." }
            Write-Host "  [ws] event #$eventCount : type='$typeStr' content=$preview"
        }
    }
}

# --- Step 4: Report results ---
Write-Host ""
Write-Host "=== E2E Stop Response Results ==="
Write-Host "  Total WS events received:     $eventCount"
Write-Host "  Chunks received before stop:  $deltaCount"
if ($firstDeltaTime -and $stopSentTime) {
    $stopLatency = ($stopSentTime - $firstDeltaTime).TotalMilliseconds
    Write-Host "  First delta -> stop sent:     ${stopLatency}ms (should be ~0)"
}
if ($stopSentTime -and $stoppedTime) {
    $stopResponseMs = ($stoppedTime - $stopSentTime).TotalMilliseconds
    Write-Host "  Stop sent -> stopped received: ${stopResponseMs}ms"
    if ($stopResponseMs -lt 1000) {
        Write-Host "  VERDICT: PASS (< 1s)"
    } elseif ($stopResponseMs -lt 3000) {
        Write-Host "  VERDICT: ACCEPTABLE (< 3s)"
    } else {
        Write-Host "  VERDICT: SLOW (> 3s)"
    }
} elseif ($stopSent -and -not $stoppedTime) {
    Write-Host "  Stop sent -> stopped received: TIMEOUT (never received stopped)"
    Write-Host "  VERDICT: FAIL"
} else {
    Write-Host "  No chunks received - agent may not have started streaming"
    Write-Host "  VERDICT: INCONCLUSIVE"
}

# Cleanup
$ws.Dispose()
$cts.Dispose()

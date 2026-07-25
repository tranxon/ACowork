# Embed 心跳超时 & Gateway 卡死 — 修改方案

> **日期**: 2026-06-27  
> **状态**: Draft  
> **作者**: Senior Engineer Agent  
> **关联故障**: 2026-06-27 13:25 Gateway 卡死，前端无法交互

## 1. 故障摘要

2026-06-27 13:25，Gateway 出现以下连锁故障：

1. LSP install `python.sh` 脚本阻塞 tokio worker 线程 47 秒
2. Embed supervisor 心跳看门狗被饿死，无法定时检查心跳
3. 看门狗恢复运行后检测到"心跳超时"（实际已过期 48s），误杀正常的 embed 进程
4. 新 embed 进程的 ONNX Runtime 初始化失败（`Encountered unknown exception in Initialize()`）
5. Gateway 在连接 embed 服务时阻塞，HTTP API 完全不可用
6. 前端无法交互，provider/模型列表刷不出来

## 2. 根因分析

### 2.1 LSP install 阻塞 tokio worker 线程（主因）

**文件**: `core/acowork-gateway/src/lsp/mod.rs` — `lsp_install_run()`

```rust
// 当前代码 — 阻塞调用
let result: std::io::Result<std::process::Output> = if cfg!(windows) {
    std::process::Command::new("powershell")
        .args(["-ExecutionPolicy", "Bypass", "-NoProfile", "-File"])
        .arg(&script_path)
        .output()           // ← 阻塞当前 tokio worker 线程
} else {
    std::process::Command::new("bash")
        .arg(&script_path)
        .output()           // ← 阻塞当前 tokio worker 线程
};
```

`std::process::Command::output()` 是同步阻塞调用，在 axum async handler 中直接执行会霸占整个 tokio worker 线程。`python.sh` 执行了 47 秒，期间该 worker 线程无法处理任何其他任务。

### 2.2 Embed supervisor 看门狗饿死（连锁反应）

**文件**: `core/acowork-gateway/src/lifecycle/embed_supervisor.rs` — `run_monitor_session()`

看门狗每 2s tick 一次，检查心跳是否超过 10s 未更新。但 `tokio::select!` 的公平性依赖于 tokio runtime 的调度。当 worker 线程被阻塞时，看门狗 task 无法被调度执行。

日志证据：`elapsed_secs=48` — 心跳实际已过期 48 秒才被检测到，而非 10 秒。这说明看门狗被饿了约 38 秒（恰好与 python.sh 的 47 秒阻塞时间吻合）。

### 2.3 误杀 embed 进程

Supervisor 检测到"心跳超时"后，执行了 kill + restart。但 embed 进程实际上是正常的 — 心跳一直在发送，只是 gateway 的看门狗没能及时检查。

### 2.4 ONNX Runtime 初始化失败

旧 embed 进���被 SIGKILL 杀死后，ONNX Runtime 的内部资源可能未完全释放。新进程初始化时遇到 `Encountered unknown exception in Initialize()`。

### 2.5 Gateway 启动时阻塞在 embed 连接

Gateway 启动时同步等待 embed 进程 spawn 完成。如果 embed 进程启动了但 ONNX Runtime 初始化失败，Gateway 在连接 embed 服务时阻塞，HTTP API 完全不可用。

## 3. 修改方案

### 3.1 P0: LSP install 改用异步进程 + idle 超时

**文件**: `core/acowork-gateway/src/lsp/mod.rs`  
**函数**: `lsp_install_run()`

**问题**: `std::process::Command::output()` 阻塞 tokio worker 线程  
**方案**: 改用 `tokio::process::Command` 异步启动子进程，逐行读取 stdout/stderr，基于"无输出时间"判定卡死

**设计思路**:

绝对超时（如 300s）本质上是在猜一个"合理上限"，但无论设多大都能找到反例 — java.sh 下载 200MB 在慢网络下可能要 5 分钟，设 300s 刚好卡住；设 600s 又对真正卡死的脚本反应太慢。

正确的设计是 **idle timeout**：只要脚本还在产出输出（stdout/stderr 有新行），就说明它没卡死，计时器重置。只有当脚本在指定时间内**没有任何输出**时，才认为卡死。这样：
- 下载 200MB 花了 10 分钟但 `curl --progress-bar` 持续输出进度 → 不超时
- `npm install` 卡在网络握手 60 秒没有任何输出 → 超时，kill 进程

idle timeout 设为 **60s** — 一个安装脚本如果 60 秒没有任何输出，大概率是卡死了。这个值与安装类型无关，不需要为每种 LSP 估算。

```rust
use tokio::process::Command;
use tokio::io::{AsyncBufReadExt, BufReader};
use std::process::Stdio;
use std::time::Duration;

/// Idle timeout: if the script produces no stdout/stderr output for this
/// duration, it is considered stuck and will be killed.
const LSP_INSTALL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Run an LSP install script with idle-timeout detection.
///
/// Unlike an absolute timeout, this only fires when the script has been
/// silent (no stdout/stderr output) for `idle_timeout` — a long download
/// that keeps printing progress will never time out.
async fn run_install_script(
    script_path: &std::path::Path,
) -> Result<InstallScriptOutput, InstallScriptError> {
    let mut child = if cfg!(windows) {
        Command::new("powershell")
            .args(["-ExecutionPolicy", "Bypass", "-NoProfile", "-File"])
            .arg(script_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)  // safety: kill child if we are dropped
            .spawn()?
    } else {
        Command::new("bash")
            .arg(script_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?
    };

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let mut last_activity = tokio::time::Instant::now();

    loop {
        // Reset the idle deadline each iteration — if we read any line,
        // last_activity is updated and the deadline moves forward.
        let idle_deadline = last_activity + LSP_INSTALL_IDLE_TIMEOUT;

        tokio::select! {
            // stdout line
            line = stdout_lines.next_line() => {
                match line {
                    Ok(Some(text)) => {
                        stdout_buf.push_str(&text);
                        stdout_buf.push('\n');
                        last_activity = tokio::time::Instant::now();
                    }
                    Ok(None) => {
                        // stdout EOF — wait for process exit
                        break;
                    }
                    Err(e) => {
                        stderr_buf.push_str(&format!("stdout read error: {e}\n"));
                        break;
                    }
                }
            }
            // stderr line
            line = stderr_lines.next_line() => {
                match line {
                    Ok(Some(text)) => {
                        stderr_buf.push_str(&text);
                        stderr_buf.push('\n');
                        last_activity = tokio::time::Instant::now();
                    }
                    Ok(None) => {
                        // stderr EOF — keep reading stdout
                    }
                    Err(e) => {
                        // Non-fatal: some tools write binary to stderr
                    }
                }
            }
            // Idle timeout — no output for LSP_INSTALL_IDLE_TIMEOUT
            _ = tokio::time::sleep_until(idle_deadline) => {
                tracing::warn!(
                    idle_secs = LSP_INSTALL_IDLE_TIMEOUT.as_secs(),
                    "LSP install script idle timeout — killing process"
                );
                let _ = child.kill().await;
                return Err(InstallScriptError::IdleTimeout {
                    idle_secs: LSP_INSTALL_IDLE_TIMEOUT.as_secs(),
                    stdout: stdout_buf,
                    stderr: stderr_buf,
                });
            }
        }
    }

    let status = child.wait().await?;
    Ok(InstallScriptOutput {
        success: status.success(),
        exit_code: status.code(),
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}
```

HTTP handler 调用：

```rust
match run_install_script(&script_path).await {
    Ok(output) => {
        let code = if output.success { StatusCode::OK } else { StatusCode::INTERNAL_SERVER_ERROR };
        (code, Json(serde_json::json!({
            "language": canonical,
            "success": output.success,
            "exit_code": output.exit_code,
            "stdout": output.stdout,
            "stderr": output.stderr,
        }))).into_response()
    }
    Err(InstallScriptError::IdleTimeout { idle_secs, stdout, stderr }) => {
        (StatusCode::GATEWAY_TIMEOUT, Json(serde_json::json!({
            "error": format!("LSP install script idle for {}s — likely stuck", idle_secs),
            "code": 504,
            "stdout": stdout,
            "stderr": stderr,
        }))).into_response()
    }
    Err(InstallScriptError::Spawn(e)) => {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "error": format!("Failed to run install script: {}", e),
            "code": 500
        }))).into_response()
    }
}
```

**为什么不需要 `spawn_blocking`**:

`tokio::process::Command` 是异步的 — `spawn()` 立即返回，`next_line()` 是 async，`child.wait()` 也是 async。整个函数运行在 tokio runtime 上，不阻塞任何 worker 线程。与 `std::process::Command::output()` 的关键区别在于：后者同步等待进程结束，前者异步逐行读取。

**改动量**: ~80 行（含错误类型定义）  
**风险**: 低 — 业务逻辑不变（仍然是 bash/powershell 执行脚本），仅改变进程管理和输出读取方式

### 3.2 P0: Embed supervisor 心跳超时增加"看门狗活性检查"

**文件**: `core/acowork-gateway/src/lifecycle/embed_supervisor.rs`  
**函数**: `run_monitor_session()`

**问题**: 看门狗本身被饿死后，恢复运行时会产生误判（把正常的 embed 当成卡死）  
**方案**: 在检测到心跳超时时，先做一次"活性探针"（HTTP /health 请求），只有探针也失败才确认 embed 卡死

```rust
// 修改 HeartbeatTimeout 处理逻辑
MonitorExit::HeartbeatTimeout => {
    // 先做活性探针 — embed 可能正常，只是看门狗被饿了
    let probe = super::embed::check_embed_health(port).await;
    if probe.is_some() {
        tracing::warn!(
            elapsed_secs = last_heartbeat.elapsed().as_secs(),
            "Embed heartbeat timeout detected, but /health probe succeeded — \
             likely watchdog starvation, not embed stuck. Reconnecting without kill."
        );
        // 不杀进程，直接重连 SSE
        continue;  // 回到 loop 顶部，重新 run_monitor_session
    }
    // 探针也失败 — embed 确实卡死，执行 kill + restart
    tracing::warn!("Embed heartbeat timeout — /health probe also failed, killing stuck process");
    // ... 原有 kill 逻辑
}
```

**改动量**: ~15 行  
**风险**: 低 — 增加一层确认，不改变原有 kill 逻辑的触发条件

### 3.3 P1: Embed 进程 ONNX Runtime 初始化重试

**文件**: `core/acowork-embed/src/main.rs` — `load_model_into_state()`  
**文件**: `core/acowork-embed/src/model.rs` — `EmbeddingModel::load()`

**问题**: ONNX Runtime `Initialize()` 偶发失败，进程启动后模型不可用  
**方案**: 在 `try_load_model` 中增加重试（最多 3 次，间隔 2s）

```rust
// model.rs — EmbeddingModel::load 增加重试
pub fn load(
    model_id: &str,
    onnx_path: &Path,
    tokenizer_path: &Path,
    pooling: PoolingStrategy,
    dimension: usize,
    max_tokens: usize,
) -> Result<Self, ModelError> {
    // ... tokenizer 加载不变 ...

    let mut last_err = None;
    for attempt in 1..=3 {
        tracing::info!(attempt, "Loading ONNX model...");
        let mut builder = Session::builder()
            .map_err(|e| ModelError::Session(format!("Failed to create session builder: {e}")))?;

        match builder.commit_from_file(onnx_path) {
            Ok(session) => {
                if attempt > 1 {
                    tracing::info!(attempt, "ONNX model loaded after retry");
                }
                return Ok(Self { /* ... */ });
            }
            Err(e) => {
                tracing::warn!(attempt, error = %e, "ONNX session load failed, will retry");
                last_err = Some(e);
                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        }
    }
    Err(ModelError::Session(format!(
        "Failed to load ONNX model after 3 attempts: {:?}",
        last_err
    )))
}
```

**改动量**: ~20 行  
**风险**: 低 — 重试逻辑独立，不影响正常路径

### 3.4 P1: Embed supervisor 增加"推理健康检查"

**文件**: `core/acowork-gateway/src/lifecycle/embed_supervisor.rs`

**问题**: 当前只检查心跳（进程活着）和 /health（HTTP 活着），但不检查推理功能是否可用  
**方案**: 在 `bootstrap_state_from_health` 之后，增加一次推理探针（POST /v1/embeddings）

```rust
// 新增函数
async fn probe_inference(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let body = serde_json::json!({"input": "health check"});
    match client.post(&url).json(&body).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

// 在 run_monitor_session 的 SSE 连接建立后调用
if !probe_inference(port).await {
    tracing::warn!("Embed inference probe failed — model may not be loaded");
    // 不重启，只是记录警告。Agent 端已有 fallback 到文本搜索的逻辑。
}
```

**改动量**: ~20 行  
**风险**: 低 — 只读探针，不改变状态

### 3.5 P1: Gateway 启动 embed 不阻塞主流程

**文件**: `core/acowork-gateway/src/gateway/mod.rs` — `run()`

**问题**: Gateway 启动时同步等待 embed spawn，如果 embed 有问题会拖慢 gateway 启动  
**方案**: embed spawn 已经是 fire-and-forget（spawn 返回 child），但 supervisor 的 startup grace 窗口（10s）会阻塞。将 supervisor 改为完全异步 — 不等待 startup grace，直接进入监控循环

```rust
// 当前代码中 supervisor 的 startup grace 会在 run_supervisor 中循环等待
// 修改：缩短 startup grace 到 5s，并在 grace 超时时不阻塞 gateway HTTP 启动
// supervisor 已经在 tokio::spawn 中运行，不阻塞主流程
// 但需要确保 HTTP server 在 embed 不可用时仍能启动

// gateway/mod.rs run() 中，HTTP server 启动不应依赖 embed 就绪
// 当前代码已经是先 spawn embed → spawn supervisor → start HTTP server
// 所以 HTTP server 启动本身不阻塞。问题在于 supervisor 内部的 grace 窗口
// 如果 embed 在 grace 窗口内未就绪，supervisor 会一直等待
// 修改：grace 窗口超时后，supervisor 进入正常监控模式，不阻塞
```

**改动量**: ~5 行（调整 STARTUP_GRACE 常量和超时后的行为）  
**风险**: 中 — 需要确保 embed 未就绪时 HTTP API 能正确返回 "embed unavailable"

### 3.6 P2: LSP install 脚本增加超时 kill

**文件**: `core/acowork-gateway/src/lsp/mod.rs`

**问题**: 即使用了 `spawn_blocking`，脚本本身也可能无限挂起  
**方案**: 在脚本内部增加 `timeout` 命令包裹

```bash
# 在每个 lsp_install/*.sh 的关键安装命令前加 timeout
timeout 30 npm install -g typescript-language-server typescript || exit 1
```

gateway 侧的 idle 超时已在 3.1 中实现，脚本内部的 timeout 作为第二道防线。

**改动量**: 每个 .sh 文件改 1-2 行  
**风险**: 低

## 4. 实施计划

### Phase 1: 紧急修复（P0）

| # | 任务 | 文件 | 改动量 | 预计耗时 |
|---|------|------|--------|----------|
| 1 | LSP install 改用 tokio::process + idle 超时(60s) | `lsp/mod.rs` | ~80 行 | 2h |
| 2 | Embed supervisor 心跳超时增加 /health 探针确认 | `embed_supervisor.rs` | ~15 行 | 1h |
| 3 | 编写回归测试 | `tests/` | ~50 行 | 1h |
| 4 | 代码审查 + 集成测试 | — | — | 1h |

### Phase 2: 健壮性改进（P1）

| # | 任务 | 文件 | 改动量 | 预计耗时 |
|---|------|------|--------|----------|
| 5 | ONNX Runtime 初始化重试（3 次，间隔 2s） | `embed/model.rs` | ~20 行 | 1h |
| 6 | Embed supervisor 增加推理健康检查探针 | `embed_supervisor.rs` | ~20 行 | 1h |
| 7 | Gateway 启动 embed 不阻塞主流程 | `gateway/mod.rs` | ~5 行 | 30min |
| 8 | 编写回归测试 | `tests/` | ~50 行 | 1h |
| 9 | 代码审查 + 集成测试 | — | — | 1h |

### Phase 3: 长期改进（P2）

| # | 任务 | 文件 | 改动量 | 预计耗时 |
|---|------|------|--------|----------|
| 10 | LSP install 脚本内部增加 timeout | `assets/lsp_install/*.sh` | 每个 1-2 行 | 30min |
| 11 | 前端增加 embed 服务状态指示器 | `apps/acowork-desktop/` | — | 2h |
| 12 | 文档更新 | `docs/` | — | 30min |

## 5. 测试计划

### 5.1 回归测试

```rust
// tests/embed_supervisor_test.rs

#[tokio::test]
async fn test_heartbeat_timeout_with_health_probe() {
    // 模拟 embed 进程正常但看门狗被饿死的场景
    // 1. 启动一个 mock embed server（/health 返回 200, /events 发心跳）
    // 2. 阻塞 tokio runtime 30 秒（模拟 LSP install 阻塞）
    // 3. 验证 supervisor 不会误杀 embed 进程
}

#[tokio::test]
async fn test_lsp_install_idle_timeout() {
    // 模拟 LSP install 脚本卡死（无输出）
    // 1. 创建一个 `sleep 120` 的脚本（不产生任何输出）
    // 2. 调用 run_install_script
    // 3. 验证 60s 后返回 IdleTimeout
}

#[tokio::test]
async fn test_lsp_install_long_running_not_timeout() {
    // 模拟长时间运行但有持续输出的脚本
    // 1. 创建一个每 5s echo 一行的脚本，运行 90s
    // 2. 调用 run_install_script
    // 3. 验证不会因为总时长 > 60s 而超时
}

#[tokio::test]
async fn test_onnx_load_retry() {
    // 模拟 ONNX Runtime 初始化失败后重试成功
    // 1. mock Session::builder().commit_from_file 第一次失败、第二次成功
    // 2. 验证 load() 在重试后返回 Ok
}
```

### 5.2 手动验证

1. **复现故障场景**: 启动 gateway + embed，执行 `POST /api/lsp/install/python`，同时发送聊天消息
2. **验证 P0 修复**: LSP install 不再阻塞其他请求；embed 不会被误杀
3. **验证 P1 修复**: 手动 kill embed 进程，观察 supervisor 重启 + ONNX 重试
4. **验证降级**: embed 不可用时，agent memory 回退到文本搜索，gateway HTTP API 正常

## 6. 风险评估

| 风险 | 概率 | 影响 | 缓解措施 |
|------|------|------|----------|
| spawn_blocking 线程池耗尽 | 低 | LSP install 排队 | tokio 默认 512 个 blocking 线程，足够 |
| /health 探针误判 | 极低 | embed 卡死不被发现 | 探针超时 2s，卡死的 embed 无法响应 |
| ONNX 重试延迟启动 | 低 | embed 启动慢 6s | 3 次 × 2s = 最多 6s 额外延迟 |
| 推理探针消耗资源 | 极低 | 每次连接多一次推理 | 探针输入仅 3 个词，耗时 <10ms |

## 7. 附录

### 7.1 故障时间线

```mermaid
timeline
    title 2026-06-27 故障时间线
    section 旧 Gateway
        13:02:30 : Embed进程启动 正常工作
        13:24:38 : Embed心跳最后一次正常
        13:24:39 : 前端 POST /api/lsp/install/python
        13:25:18 : Agent正常完成响应
        13:25:26 : 看门狗恢复 检测到48s超时
        13:25:26 : 误杀embed进程(PID 80703)
        13:25:26 : python.sh超时47s返回500
        13:26:00 : Gateway日志停止
    section 新Gateway-1
        13:29:59 : Gateway重启
        13:30:02 : 连接embed后日志停止
    section 新Gateway-2
        13:31:02 : Gateway再次重启
        13:31:05 : ONNX Runtime初始化失败
        13:34:50 : Embedding失败 回退文本搜索
        13:42:38 : HTTP handler超时
        13:43:33 : 日志结束
```

### 7.2 受影响文件清单

| 文件 | 修改类型 | Phase |
|------|----------|-------|
| `core/acowork-gateway/src/lsp/mod.rs` | 修改 `lsp_install_run` | P0 |
| `core/acowork-gateway/src/lifecycle/embed_supervisor.rs` | 修改 `run_monitor_session` + 新增 `probe_inference` | P0 + P1 |
| `core/acowork-embed/src/model.rs` | 修改 `EmbeddingModel::load` | P1 |
| `core/acowork-gateway/src/gateway/mod.rs` | 调整 startup grace | P1 |
| `assets/lsp_install/*.sh` | 增加 timeout | P2 |
| `core/tests/` | 新增回归测试 | P0 + P1 |

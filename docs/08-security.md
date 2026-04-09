# 安全设计

> 版本：v3.0 | 更新日期：2026-04-09

---

## 1. 进程隔离

- 每个 Agent 独立进程，一个崩溃不影响其他。
- Agent Runtime 是平台信任的二进制，.agent 包无可执行代码。

## 2. 文件系统隔离

- Agent 只能写入自己的工作区目录和用户明确授权的目录。
- 私有 Grafeo 文件在工作区内，沙箱层面强制隔离。

## 3. 包签名验证

- 所有 .agent 包安装前必须通过签名验证（详见 [02-agent-package.md](./02-agent-package.md)），未签名或签名无效的包拒绝安装。
- 系统 Agent（manifest 中 `system: true`）必须是 Platform 签名，Gateway 内置平台根公钥用于验证。
- Agent 升级时，新包的签名证书指纹必须与已安装版本一致，防止恶意包覆盖。
- Gateway 配置中可定义基于签名者的信任规则（`trusted_signers`），特定签名的 Agent 可自动获得额外权限。
- 签名覆盖整个 ZIP 内容（Local Files + Central Directory + End of Directory），不存在未签名区域。

## 4. 网络隔离

- 默认禁止网络（bwrap `--unshare-net`），仅按 manifest 授权域名配置代理或白名单。
- LLM API 调用需要 `network:https://api.openai.com` 等显式声明。

## 5. 权限最小化

- manifest 必须声明所有权限，用户安装时可拒绝。
- 运行时权限请求：Agent 可通过 Gateway 请求额外权限，Gateway 弹出对话框让用户确认。

```json
{
  "type": "permission_request",
  "permission": "filesystem:read:~/Downloads",
  "reason": "需要读取下载的 CSV 文件进行分析"
}
```

## 6. API Key 安全

- Key 集中存储在 Gateway Vault（加密）。
- 不通过环境变量分发（避免 ps/procfs 泄露）。
- Agent Runtime 启动后通过 Unix Socket 一次性获取，存于进程内存。
- Agent Runtime 是可信二进制，.agent 包无可执行代码，WASM 工具在沙箱内无法读取宿主内存。

## 7. WASM 工具沙箱

- 自定义工具以 WASM 形式运行在 Wasmtime 沙箱中。
- 天然内存隔离、系统调用限制、资源限制（max_memory_mb, max_execution_time_ms）。
- 无法访问宿主进程内存、文件系统、网络。

## 8. 沙箱强化

- Linux：seccomp-bpf 限制危险系统调用（clone、ptrace 等）。
- bubblewrap 提供文件系统级隔离。

## 9. Prompt Injection 防护

- Agent Runtime 内置 Prompt Guard（参考 ZeroClaw），检测和过滤可疑输入。
- 高风险工具执行（文件写入、网络请求、Intent 发送）需用户确认（approval 机制）。
- 审计日志：Agent 发出的所有工具调用和 Intent 都被记录和可回溯。

## 10. Memory 传输加密

- 云端同步使用 HTTPS / gRPC TLS。
- 本地 Grafeo 文件可选加密（使用用户密钥派生）。

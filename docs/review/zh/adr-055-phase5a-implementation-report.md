# ADR-055 Phase 5a 实施报告：安全模型（第一档）

**日期**：2026-08-26
**状态**：Phase 5a 全部完成，全链路编译/测试/clippy 通过（未提交，待确认）
**ADR**：`docs/adr/zh/ADR-055-remote-runtime-node-topology.md`（§6.8、§7）

---

## 交付清单

### 1. 协议层（acowork-core）

- **修改** `core/acowork-core/proto/mqtt_payload.proto`
  - `DataEnvelope.payload` oneof 新增字段 **85**：`NodeEnroll node_enroll`
  - 新增字段 **86**：`NodeEnrollResult node_enroll_result`
  - 新增 `NodeEnroll`（node_id、machine_uid、os、arch、node_version、protocol_version、capabilities、enrollment_token）与 `NodeEnrollResult`（node_id、machine_uid、node_token、status、message）
- **修改** `core/acowork-core/src/node.rs` — `node_enroll_topic(node_id)` / `node_enroll_result_topic(node_id)` 常量函数 + 单测
- **修改** `core/acowork-core/tests/node_proto_golden.rs` — NodeEnroll/NodeEnrollResult golden 测试（该文件共 8 个测试全部通过）

### 2. Gateway 侧（acowork-gateway）

- **新建** `src/mqtt/enrollment.rs`（454 行，8 测试）
  - `EnrollmentTokenStore`：`{data_dir}/enrollment_tokens.json` 持久化，存 sha256 哈希不存明文（created_at / expires_at / consumed_by）；`create_token(ttl)` 明文一次性打印；常量时间比较；一次性消费
  - `NodeTokenStore`：`{data_dir}/node_tokens.json`（node_id → {token, machine_uid, created_at}），Gateway 重启后已注册 node 可凭 node_token 重连
  - `TokenValidation` 枚举（Valid / Expired / Unknown / Consumed）
- **修改** `src/config.rs` — `mqtt.auth_enabled`（默认 **false**，保持现状；开启后全链路鉴权生效）
- **修改** `src/mqtt/broker.rs`（9 测试）
  - `start_broker` 支持 `Option<AuthHandler>`（rumqttd 0.20 `set_auth_handler`）
  - 纯函数 `check_connect_auth(client_id, username, password, ctx)` 决策规则：
    - auth_enabled=false → 全部放行
    - `node:{id}`：password == node_token(id) 或有效未消费 enrollment token → 放行
    - `agent:{id}`：password == 任一已注册 node_token → 放行（第一档简化：不校验 agent→node 归属，注释说明）
    - `gateway:publisher`：password == 内部 publisher token（启动时生成）
    - `user:*:desktop:*`：password == http_token（auth_enabled 时 HttpAuth 已生成）
    - 其他 → 拒绝
- **修改** `src/mqtt/dispatch.rs`（14 测试）— 订阅 `acowork/nodes/+/enroll`；处理：token 校验（开启时）→ node_id 唯一性（未占用 / 同 machine_uid 复用 / 不同 machine_uid 拒绝报错）→ 签发或复用 node_token（持久化 + NodeRegistry 记录）→ 回执 `enroll_result`。测试覆盖幂等 / 重名拒绝 / token 签发
- **修改** `src/mqtt/node_registry.rs`（9 测试）— `NodeInfoState` 增加 node_token 槽（enroll 结果写入）
- **修改** `src/cli.rs` — `nodes token create [--ttl]` 真实实现（调 EnrollmentTokenStore，明文一次性打印）
- **修改** `src/gateway/node_manager.rs` — local node spawn 增加 `--token`（Gateway 预签发 local node token 持久化进 NodeTokenStore）；`ensure_local_node` 前确保 local token 存在
- **修改** `src/mqtt/client.rs` — Gateway MQTT client `connect` 加 username/password（publisher 内部 token）
- **HTTP 通道鉴权**（auth_enabled 时生效）：
  - `src/http/agents.rs` `download_package`：校验 `X-ACowork-Node-Token`（NodeTokenStore 匹配）→ 401/403
  - `src/http/proxy.rs`：按 agent_id → installed_agents.node_id → node registry 取 token → 出站请求带 `X-ACowork-Node-Token` header
  - `/api/status`：auth_enabled 时返回 `mqtt_username` / `mqtt_password`（Desktop MQTT 凭据下发）

### 3. Node 侧（acowork-node）

- **修改** `src/control/mqtt.rs` — `connect` 加 username/password（username=`node:{node_id}`，password=node_token 或 `--token` 传入的 enrollment token）
- **修改** `src/control/mod.rs`（8 测试）
  - bootstrap 发布 `acowork/nodes/{node_id}/enroll`（携带 token）→ 订阅 `enroll_result` → 回执 node_token 持久化进 identity.json（激活 `set_node_token`）
  - enroll 幂等：已注册（同 machine_uid）重连时 Gateway 复用 node_token，Node 只读不回写（token 为空才写）
- **修改** `src/proxy/mod.rs`（2 测试）— 校验入站 `X-ACowork-Node-Token` == identity.node_token（常量时间比较；有 token 则必须匹配，未 enroll 放行）；403 + `X-Error-Origin: node`
- **修改** `src/process/spawn.rs` / `src/process/manager.rs` — Node 持有 node_token 时，spawn Runtime 注入 `--mqtt-username agent:{id}` / `--mqtt-password {node_token}`
- **修改** `src/cli.rs` — `start` / `enroll` 的 `--token` 生效（传至 CONNECT 凭据 + enroll payload）

### 4. Runtime 侧（acowork-runtime）

- `MqttConnectConfig` 加 `username` / `password: Option<&str>`
- CLI 加 `--mqtt-username` / `--mqtt-password`（Node spawn 注入；standalone 可手动提供）
- `src/mqtt/client.rs`（4 测试）— 有凭据时 `set_credentials`，无凭据保持现状

### 5. Desktop（apps/acowork-desktop）

- `mqtt_client.rs` `connect_mqtt`：从 `/api/status` 读取 `mqtt_username` / `mqtt_password`（存在时设置凭据；mqtt_port 动态化 Phase 1.3 已完成，仅补凭据字段）

### 6. 测试

- **单测新增**：enrollment store（创建/校验/过期/一次性消费/持久化重载）、auth handler 决策纯函数、dispatch enroll 处理（幂等/重名拒绝/签发）、NodeEnroll golden、Node proxy 鉴权、enroll 回执持久化
- **e2e 扩展** `core/acowork-gateway/tests/node_control_plane_e2e.rs`（529 行，3 个测试全部通过）：
  - `node_binary_speaks_the_control_plane_contract`（原有扩展）
  - `auth_broker_rejects_uncredentialed_node_connects`（新增）：auth_enabled 下无凭据/错凭据 CONNECT 被拒（CONNACK 5）
  - `node_enrolls_and_reconnects_with_node_token_under_auth`（新增）：完整闭环——enrollment token 连接 → enroll 发布 → Gateway 侧 validate/consume/upsert → enroll_result 回执 → node_token 持久化 → 进程重启后用 node_token 凭据重连成功
  - 隔离：auth 场景用独立端口（18991/18992/19901），避免与 verify 测试的 19900 冲突
  - 已知竞态处理：kill 后 broker 必发 retained LWT offline，状态等待循环跳过 offline 直到 online

### 7. 文档

- `docs/zh/protocols/mqtt.md`：§1 认证行、§3.6 节点控制面（enroll/enroll_result topic + enrollment 语义）、§8.5 Client ID 表、§8.7 CONNECT 层鉴权规则表 + 一键接入流程、§13 源码索引
- `docs/zh/protocols/http.md`：§4.1 /api/status 凭据字段、§4.2 download 鉴权行、§5 反代注入说明
- `docs/adr/zh/ADR-055-remote-runtime-node-topology.md`：状态行更新（Phase 1–5a 完成）、§6.2 topic 表、§6.8 实现状态 + 偏差记录、§6.12 enroll 流程、§7 Phase 5 表

---

## 验证结果

| 检查 | 结果 |
|------|------|
| `cargo clippy --workspace --exclude acowork-embed --all-targets -- -D warnings` | ✅ 零告警（修复 broker.rs manual_strip、dispatch.rs unnecessary_map_or） |
| `cargo test --workspace --exclude acowork-embed` | ✅ 44 组 test result 全部 ok |
| e2e `node_control_plane_e2e`（3 个测试） | ✅ 全部通过（含 auth 两个新场景） |
| Desktop `tsc --noEmit` | ✅ 零错误 |
| Desktop `vitest run` | ✅ 185 passed |
| Desktop `src-tauri cargo check` | ✅ 通过 |

**默认路径回归**：auth_enabled=false 时全链路行为不变（默认配置未改动，CONNECT 全部放行、HTTP 通道不校验），单机部署不受影响。

---

## 已知限制与偏差

- **topic 级 ACL 未落地**：rumqttd 0.20 无 per-topic 鉴权能力，Phase 5a 仅实现 CONNECT 层动态鉴权；mosquitto 评估列入 Phase 5b（ADR-055 §6.8 已记录）
- **agent→node 归属未校验**：`agent:{id}` 凭据仅校验 password 为任一已注册 node_token，不校验具体归属（第一档简化，注释说明）
- Desktop HTTP API 鉴权（HttpAuth 路由接入、Desktop Bearer 支持）不在本阶段，列为后续项

**预先存在的环境问题（与本次无关）**：`acowork-embed` 链接失败（ort-sys 找不到 libonnxruntime，本机未配置 ONNX Runtime），全量回归使用 `--exclude acowork-embed` 绕过。

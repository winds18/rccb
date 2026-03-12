# RCCB 功能规格说明

## 1. 重构目标

`rccb` 以 `ccb` 源码为依据重构核心链路，优先保证通信可靠性和执行一致性。

目标：

1. 全 Rust 主实现
2. 项目级目录绑定（禁用全局主状态）
3. 编排模型: 首 provider 编排、其余执行
4. 本地 daemon 协议稳定（`ask.*`）
5. 会话/任务/临时目录全部落在 `.rccb/`
6. 源码多文件工程化，交付单文件二进制
7. 可动态启停调试开关并捕获完整调试日志

## 2. 项目目录模型

根目录: `<project>/.rccb/`

- `run/`: 进程状态与锁
- `sessions/`: 会话主文件和 provider 角色文件
- `tasks/`: 请求任务生命周期
- `tmp/`: provider 临时目录
- `logs/`: daemon/provider 日志

## 3. 编排模型

输入 providers（如 `claude codex gemini opencode droid`）：

1. 第 1 个 provider => `orchestrator`
2. 第 2..N 个 provider => `executors`

落盘：

1. `sessions/<instance>/session.json`
2. `sessions/<instance>/providers/<provider>.json`
3. `tasks/<instance>/task-*.json`
4. `tmp/<instance>/<provider>/`

## 3.1 Provider 执行模型

`RCCB_EXEC_MODE` 支持三种模式：

1. `ccb`（默认）
   - 通过 CCB 包装命令执行 provider：
     - `codex -> cask`
     - `gemini -> gask`
     - `opencode -> oask`
     - `droid -> dask`
     - `claude -> lask`
   - 继承 CCB 会话发现/路由能力，兼容 wezterm 与 tmux 会话。
   - 命令路径可覆盖：
     - `RCCB_<PROVIDER>_CMD`
     - `RCCB_CCB_BIN_DIR`
     - `RCCB_CCB_ROOT`
2. `native`（实验中）
   - Rust 直接启动 provider 本地进程（`claude/codex/gemini/opencode/droid`）。
   - 路径解析优先项目内绑定：
     - `<project>/.rccb/bin/<provider>`
     - `<project>/bin/<provider>`
     - 之后才回退到系统 `PATH`
   - 支持项目级 profile：
     - `<project>/.rccb/providers/<provider>.json`
     - 字段：`cmd`（可选）、`args`（可选）、`timeout_s`（可选）、`quiet`（可选）、`no_wrap`（可选）、`env`（可选）
   - 可覆盖二进制与参数：
     - `RCCB_<PROVIDER>_NATIVE_CMD`
     - `RCCB_<PROVIDER>_NATIVE_ARGS`
     - `RCCB_NATIVE_BIN_DIR`
     - `RCCB_NATIVE_ARGS`
   - 可关闭自动 prompt 包装（高级用法）：
      - `RCCB_NATIVE_NO_WRAP`
      - `RCCB_<PROVIDER>_NATIVE_NO_WRAP`
   - 可覆盖原生执行策略：
      - `RCCB_NATIVE_TIMEOUT_S` / `RCCB_<PROVIDER>_NATIVE_TIMEOUT_S`
      - `RCCB_NATIVE_QUIET` / `RCCB_<PROVIDER>_NATIVE_QUIET`
   - `args` 模板变量：
      - `{req_id}` / `{caller}` / `{provider}` / `{timeout_s}` / `{work_dir}`
   - `env` 值模板变量：
      - `{req_id}` / `{caller}` / `{provider}` / `{timeout_s}` / `{work_dir}`
   - 优先级（高 -> 低）：
      - cmd: `RCCB_<PROVIDER>_NATIVE_CMD` -> profile `cmd` -> `RCCB_NATIVE_BIN_DIR` -> `.rccb/bin` -> `bin` -> `PATH`
      - args: `RCCB_<PROVIDER>_NATIVE_ARGS` -> `RCCB_NATIVE_ARGS` -> profile `args`
      - timeout: `RCCB_<PROVIDER>_NATIVE_TIMEOUT_S` -> `RCCB_NATIVE_TIMEOUT_S` -> profile `timeout_s` -> request `timeout_s`
      - quiet: `RCCB_<PROVIDER>_NATIVE_QUIET` -> `RCCB_NATIVE_QUIET` -> profile `quiet` -> request `quiet`
      - wrap: `RCCB_<PROVIDER>_NATIVE_NO_WRAP` -> `RCCB_NATIVE_NO_WRAP` -> profile `no_wrap`
   - 成功判定更严格：`exit_code=0` 且输出包含 `CCB_DONE: <req_id>` 才视为 `completed`；否则记为 `incomplete`（`exit_code=2`）。
3. `stub`（开发联调）
   - 仅用于通信链路调试，不用于真实 provider 执行。

请求级环境变量注入（所有模式）：

1. `CCB_CALLER`
2. `CCB_REQ_ID`

## 4. daemon 协议（核心）

### 4.1 消息类型

1. `ask.ping` -> `ask.pong`
2. `ask.request` -> `ask.response`（非流式）
3. `ask.request(stream=true)` -> `ask.event(start|delta|done|error)`（流式）
4. `ask.request(async=true)` -> `ask.response(submitted)`（异步提交）
5. `ask.cancel` -> `ask.response`
6. `ask.shutdown` -> `ask.response`
7. `ask.debug` -> `ask.response`

### 4.2 鉴权

每条请求必须携带 token；daemon 校验失败返回 `Unauthorized`。

### 4.3 请求字段（ask.request）

- `id`
- `token`
- `provider`
- `work_dir`
- `timeout_s`
- `quiet`
- `stream`（可选，默认 `false`）
- `async`（可选，默认 `false`，与 `stream` 互斥）
- `message`
- `caller`
- `req_id`（可选）

### 4.4 响应字段（ask.response）

- `id`
- `req_id`
- `exit_code`
- `reply`
- `provider`
- `meta`

### 4.5 流式事件字段（ask.event）

- `id`
- `req_id`
- `provider`
- `event`（`start|delta|done|error`）
- `delta`（仅 `delta` 事件）
- `reply`（`done/error` 可带完整文本）
- `exit_code`（`done/error`）
- `meta`

### 4.6 可靠性机制

1. provider 级 worker 串行队列
2. 任务状态流转：`queued -> running -> completed|failed|timeout|canceled|incomplete`
3. 超时返回 `exit_code=2`
4. state 文件原子写
5. 心跳更新时间戳
6. 流式输出采用增量事件行（NDJSON）并按 `done/error` 收敛
7. 调试开关启用时写入完整协议与执行日志到 `logs/<instance>/debug.log`
8. provider 进程执行带超时控制，超时返回 `exit_code=2`
9. 可选 completion hook：任务终态后异步触发，不阻塞主请求路径

### 4.7 Completion Hook（可选）

1. 触发时机：worker 完成并生成终态响应后异步触发。
2. 命令来源（优先级）：
   - `RCCB_<PROVIDER>_COMPLETION_HOOK_CMD`
   - `RCCB_COMPLETION_HOOK_CMD`
3. 开关与超时：
   - `RCCB_COMPLETION_HOOK_ENABLED`（默认启用）
   - `RCCB_COMPLETION_HOOK_TIMEOUT_S`（默认 30s）
4. hook 输入：
   - reply 通过 `stdin` 传入
   - 上下文通过 `RCCB_HOOK_*` 环境变量传入
5. 兼容性：
   - 同步注入 `CCB_CALLER/CCB_REQ_ID/CCB_DONE_SEEN/CCB_COMPLETION_STATUS`

## 5. 命令接口

### 5.0 初始化

1. `rccb init [--force]`
2. 初始化输出：
   - `.rccb/config.example.json`
   - `.rccb/providers/*.example.json`（native profile 模板）

### 5.1 启动

1. 快捷启动: `rccb claude codex gemini opencode droid`
2. 显式启动: `rccb start [--instance] [--listen] [--task] [--debug] <providers...>`
3. 快捷启动行为：
   - 自动确保 `default` 实例 daemon 在线（后台启动）
   - 在 `tmux/wezterm` 环境自动拉起 provider CLI pane
   - 非 `tmux/wezterm` 环境仅确保 daemon 在线并提示如何继续

### 5.2 通信

1. `rccb ping --instance <id>`
2. `rccb ask --instance <id> --provider <p> --caller <c> "..."`
3. `rccb ask --instance <id> --provider <p> --caller <c> --stream "..."`
4. `rccb ask --instance <id> --provider <p> --caller <c> --async "..."`
5. `rccb cancel --instance <id> --req-id <rid>`
6. `rccb stop --instance <id>`（优先 graceful shutdown）
7. 兼容旧快捷后端指令（统一入口）：
   - ask: `rccb cask|gask|oask|lask|dask "..."`
   - ping: `rccb cping|gping|oping|lping|dping`
   - pend: `rccb cpend|gpend|opend|lpend|dpend`

### 5.3 状态

1. `rccb status [--instance <id>] [--as-json]`
2. `rccb tasks [--instance <id>] [--limit N] [--as-json]`
3. `rccb watch --instance <id> --req-id <rid> [--with-provider-log] [--with-debug-log] [--timeout-s <sec>]`
4. `status --as-json` 额外返回 `in_flight_count` 与 `in_flight_req_ids`

### 5.4 调试

1. `rccb debug on --instance <id>`
2. `rccb debug off --instance <id>`
3. `rccb debug status --instance <id>`

### 5.5 IM

1. `rccb send feishu ...`
2. `rccb send telegram ...`

## 6. 状态文件关键字段

`run/<instance>.json` 包含：

- `providers`
- `orchestrator`
- `executors`
- `daemon_host`
- `daemon_port`
- `daemon_token`
- `debug_enabled`
- `session_file`
- `last_task_id`

## 7. 验收标准

1. 仅使用项目 `.rccb`，不污染全局主状态
2. 同实例互斥、不同实例并发
3. `ask.ping/ask.request/ask.event/ask.cancel/ask.debug/ask.shutdown` 全部可用
4. 请求生命周期可追踪（tasks 文件）
5. 编排角色与落盘一致
6. 单二进制可运行

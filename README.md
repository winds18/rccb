# RCCB (Rust Claude Code Bridge)

`rccb` 是 Rust 重构版 bridge，当前聚焦 `ccb` 最核心能力：

- 项目级 `.rccb` 绑定（不走全局）
- 多 provider 编排模型（第一个编排者，其余执行者）
- 高可靠本地 daemon 通信（支持 `ask.request/ask.response` 与 `ask.event` 流式事件）
- 源码多模块工程化 + 交付单二进制运行
- 调试开关（可动态 on/off）与完整调试日志
- IM 通道（飞书、Telegram）

## 编排启动

### 1) 快捷风格

```bash
rccb claude codex gemini opencode droid
```

规则：

1. 第一个 provider 默认 `orchestrator`
2. 其余 provider 默认 `executors`

### 2) 显式 start

```bash
rccb --project-dir . start --instance team-a claude codex gemini opencode droid
```

开启完整调试日志启动：

```bash
rccb --project-dir . start --instance team-a --debug claude codex gemini
```

可选初始任务：

```bash
rccb --project-dir . start --instance team-a \
  --task "实现登录模块并补测试" \
  claude codex gemini
```

## 核心通信机制（Rust）

`rccb start` 会启动项目内 daemon，通信协议与 `ccb askd` 对齐：

- `ask.ping`
- `ask.request`
- `ask.event`（`start|delta|done|error`，仅流式请求）
- `ask.shutdown`
- `ask.response`
- `ask.debug`（调试开关）

并具备：

1. token 鉴权
2. req_id 生成与回传
3. 按 provider 的 worker 串行队列
4. 任务生命周期落盘（queued/running/completed）

### 通信命令

```bash
# ping daemon
rccb --project-dir . ping --instance team-a

# 发送请求（走 daemon 协议，不直接本地函数调用）
rccb --project-dir . ask \
  --instance team-a \
  --provider codex \
  --caller claude \
  --timeout-s 30 \
  "帮我检查这个模块的边界条件"

# 实时流式请求
rccb --project-dir . ask \
  --instance team-a \
  --provider codex \
  --caller claude \
  --stream \
  "流式输出这个任务的执行过程"

# 停止 daemon（优先 graceful shutdown）
rccb --project-dir . stop --instance team-a
```

### 调试开关与完整日志

```bash
# 查看当前调试状态
rccb --project-dir . debug status --instance team-a

# 动态开启/关闭调试（无需重启）
rccb --project-dir . debug on --instance team-a
rccb --project-dir . debug off --instance team-a
```

调试日志路径：

- `./.rccb/logs/<instance>/debug.log`

`debug.log` 会记录完整协议收发（`ask.*`）、流式事件和 worker 生命周期，便于复盘与问题反馈。
当 daemon 不在线时，`debug on/off` 会写入实例状态，并在下次 `start` 自动继承。

## Provider 执行适配

支持三种执行模式（`RCCB_EXEC_MODE`）：

1. `ccb`（默认，推荐生产）
   - 调用 CCB 包装命令：`cask/gask/oask/dask/lask`
   - 继承 CCB 会话路由能力，支持 **wezterm / tmux**
   - 命令覆盖：
     - `RCCB_CODEX_CMD`
     - `RCCB_GEMINI_CMD`
     - `RCCB_OPENCODE_CMD`
     - `RCCB_DROID_CMD`
     - `RCCB_CLAUDE_CMD`
   - CCB 安装路径：
     - `RCCB_CCB_BIN_DIR`
     - `RCCB_CCB_ROOT`
2. `native`（纯 Rust 直连 provider 进程，实验中）
   - 按 provider 查找本地二进制：`claude/codex/gemini/opencode/droid`
   - 优先支持项目内相对绑定：
     - `<project>/.rccb/bin/<provider>`
     - `<project>/bin/<provider>`
   - 支持项目级 profile：
     - `<project>/.rccb/providers/<provider>.json`
     - 字段：`cmd`、`args`、`no_wrap`
   - 原生命令覆盖：
     - `RCCB_<PROVIDER>_NATIVE_CMD`
     - `RCCB_NATIVE_BIN_DIR`
   - 原生参数覆盖：
     - `RCCB_<PROVIDER>_NATIVE_ARGS`
     - `RCCB_NATIVE_ARGS`
   - 可关闭自动 prompt 包装（高级用法）：
     - `RCCB_NATIVE_NO_WRAP=1`
     - `RCCB_<PROVIDER>_NATIVE_NO_WRAP=1`
   - `args` 支持模板变量：
     - `{req_id}`、`{caller}`、`{provider}`、`{timeout_s}`、`{work_dir}`
   - `native` 模式下，provider 成功返回必须包含 `CCB_DONE: <req_id>`；否则标记为 `incomplete`（`exit_code=2`）

   生效优先级（从高到低）：
   - 命令：`RCCB_<PROVIDER>_NATIVE_CMD` > profile `cmd` > `RCCB_NATIVE_BIN_DIR` > 项目 `.rccb/bin` > 项目 `bin` > `PATH`
   - 参数：`RCCB_<PROVIDER>_NATIVE_ARGS` > `RCCB_NATIVE_ARGS` > profile `args`
   - no-wrap：`RCCB_<PROVIDER>_NATIVE_NO_WRAP` > `RCCB_NATIVE_NO_WRAP` > profile `no_wrap`
3. `stub`（仅联调）
   - 仅用于通信链路调试，不用于真实 provider 执行

示例：

```bash
# 默认（ccb）
export RCCB_EXEC_MODE=ccb

# 纯 Rust 原生执行（实验）
export RCCB_EXEC_MODE=native
export RCCB_CODEX_NATIVE_CMD=/usr/local/bin/codex
export RCCB_CODEX_NATIVE_ARGS='--model gpt-5-codex'

# 项目级 profile（推荐用于多项目差异配置）
cat > ./.rccb/providers/codex.json <<'JSON'
{
  "cmd": "./.rccb/bin/codex",
  "args": ["--request-id", "{req_id}"],
  "no_wrap": false
}
JSON

# 联调桩模式
export RCCB_EXEC_MODE=stub
```

## 项目目录落盘

所有会话、任务、临时文件、日志都在项目内：

- `./.rccb/run/`：实例状态与锁
- `./.rccb/sessions/`：会话记录与 provider 角色
- `./.rccb/tasks/`：任务生命周期记录
- `./.rccb/tmp/`：provider 临时目录
- `./.rccb/logs/`：daemon/provider 日志

## IM 通道

```bash
# 飞书
rccb send feishu \
  --webhook-url "https://open.feishu.cn/open-apis/bot/v2/hook/xxx" \
  --text "hello from rccb"

# Telegram
rccb send telegram \
  --bot-token "123456789:xxx" \
  --chat-id "-1001234567890" \
  --text "hello from rccb"
```

## Completion Hook（可选）

任务完成后可异步触发 hook（不阻塞 worker）：

```bash
export RCCB_COMPLETION_HOOK_CMD='/path/to/hook --mode notify'
# 或按 provider 覆盖
export RCCB_CODEX_COMPLETION_HOOK_CMD='/path/to/codex-hook'
```

开关与超时：

- `RCCB_COMPLETION_HOOK_ENABLED`（默认 `1`）
- `RCCB_COMPLETION_HOOK_TIMEOUT_S`（默认 `30`，最大 `300`）

hook 进程可读取上下文环境变量：

- `RCCB_HOOK_PROVIDER`
- `RCCB_HOOK_CALLER`
- `RCCB_HOOK_REQ_ID`
- `RCCB_HOOK_STATUS`（`completed|cancelled|failed|incomplete`）
- `RCCB_HOOK_DONE_SEEN`（`1|0`）
- `RCCB_HOOK_EXIT_CODE`
- `RCCB_HOOK_INSTANCE_ID`
- `RCCB_HOOK_PROJECT_DIR`
- `RCCB_HOOK_WORK_DIR`

兼容变量（便于复用 CCB 生态脚本）：

- `CCB_CALLER`
- `CCB_REQ_ID`
- `CCB_DONE_SEEN`
- `CCB_COMPLETION_STATUS`

reply 文本通过 `stdin` 传给 hook 命令。

## 编译与单文件运行

```bash
cargo build --release
```

源码是多文件（`src/*.rs`），交付是单文件二进制。生成文件：

- `target/release/rccb`

可选交付脚本（输出到 `dist/`）：

```bash
./scripts/build-deliverable.sh
```

清理式烟测（临时目录执行，结束自动删除）：

```bash
./scripts/smoke-clean.sh
```

## 目前状态

已完成：

1. 项目级多实例绑定
2. 编排模型与角色落盘
3. Rust daemon + ask 协议通信链路（含实时 `--stream`）
4. Provider 执行三模式（`ccb`/`native`/`stub`）
5. 调试开关与完整调试日志
6. IM 通道

下一阶段：

1. provider-specific native adapter 深化（逐 provider 能力对齐）
2. completion hook 与回调兼容增强
3. 管理面（mail/web）迁移

详见 `docs/functional-spec.md`、`docs/rewrite-roadmap.md`。

# RCCB

`rccb` 是 Rust 重构版实时协同桥，当前聚焦这套桥接体系的核心能力：

- 项目级 `.rccb` 绑定（不走全局）
- 多 provider 编排模型（第一个编排者，其余执行者）
- 高可靠本地 daemon 通信（支持 `ask.request/ask.response` 与 `ask.event` 流式事件）
- 源码多模块工程化 + 交付单二进制运行
- 调试开关（可动态 on/off）与完整调试日志
- IM 通道（飞书、Telegram）

## 项目初始化

```bash
rccb --project-dir . init
```

会生成：

- `./.rccb/config.example.json`
- `./.rccb/providers/*.example.json`（native provider profile 模板）
- `./.rccb/bin/*`（provider 启动包装脚本）
- `./AGENTS.md`（跨 provider 共享协作规则，含托管区块与用户区块）
- `./CLAUDE.md`
- `./GEMINI.md`
- `./.agents/skills/rccb-delegate/SKILL.md`（Codex 技能）
- `./.agents/skills/rccb-audit/SKILL.md`
- `./.agents/skills/rccb-research-verify/SKILL.md`
- `./.opencode/skills/rccb-delegate/SKILL.md`
- `./.opencode/commands/rccb-*.md`
- `./.opencode/agents/*.md`
- `./.claude/commands/rccb-*.md`
- `./.claude/agents/*.md`
- `./.factory/skills/rccb-delegate/SKILL.md`
- `./.factory/commands/rccb-*.md`
- `./.factory/rules/rccb-core.md`
- `./.factory/droids/*.md`

规则文件策略：

- 普通模式：只补缺失文件，不覆盖已有项目级规则
- `debug` 模式：刷新 RCCB 生成模板，包括 `.rccb/config.example.json`、`providers/*.example.json` 与项目级托管规则
- 启动包装脚本也属于 RCCB 生成模板，`debug` 模式会一并刷新
- `AGENTS.md` / `CLAUDE.md` / `GEMINI.md` 会保留用户区块，方便写项目个性化规则

Codex / Gemini 强化点：

- `Codex` 额外生成审计与调研复核技能：`rccb-audit`、`rccb-research-verify`
- `GEMINI.md` 内置两轮调研、多路径交叉验证、冲突项显式输出的工作流

## 编排启动

### 1) 快捷风格

```bash
rccb claude codex gemini opencode droid
```

规则：

1. 第一个 provider 默认 `orchestrator`
2. 其余 provider 默认 `executors`
3. 自动后台拉起 `rccb start --instance default`（项目级 `.rccb` daemon）
4. 若在 `tmux/wezterm` 内运行，会自动为 provider 拉起 CLI pane（不再只写状态文件）
5. pane 布局规则：
   - `<=4` 个 provider：左侧保留 orchestrator，右侧放其余（右侧等分）
   - `=5` 个 provider：左侧上下两块（orchestrator + 1），右侧放其余（右侧等分）
6. orchestrator 退出后：`rccb` 自动退出，并停止 default 实例 daemon，清理本次派生 pane
7. 所有 provider pane 保持真实 CLI 执行视图，不显示旁路日志，不注入任务状态镜像
8. 默认采用静默后台通信：任务下发/回传不打扰 CLI 输入区；可通过命令查看状态和输出
9. `debug` 开启时会自动创建一个日志 pane（位于编排者 pane 上方），默认跟踪首个执行者并持续 `watch --follow`，所有旁路日志仅在这个 debug pane 显示
10. 默认启用 `orchestrator strict mode`：当存在执行者时，编排者 pane 会自动收到“只做思考/委派”的 guardrail；执行者完成后，若 `caller` 指向编排者，会自动把最终结果后台回注给编排者 pane
11. `debug` 不是粘滞状态：上一次即使开过 debug，这一次若未显式指定 `--debug` 或 `RCCB_DEBUG=1`，也不会自动拉起 debug pane
12. 可选开关：
   - `RCCB_WATCH_MAX_LOG_LINES=<N>`：`watch` 每次刷新最多展示 N 行日志（默认 10，避免刷屏）
   - `RCCB_CODEX_PANE_EXEC=0`：关闭 codex 的 pane 执行（默认开启；无 pane 时自动回退后台 native）
   - `RCCB_GEMINI_PANE_EXEC=0`：关闭 gemini 的 pane 执行（默认开启；无 pane 时自动回退后台 native）
   - `RCCB_OPENCODE_PANE_EXEC=0`：关闭 opencode 的 pane 执行（默认开启；无 pane 时自动回退后台 native）
   - `RCCB_DROID_PANE_EXEC=0`：关闭 droid 的 pane 执行（默认开启；无 pane 时自动回退后台 native）
   - `RCCB_DEBUG_WATCH_PANE=0`：关闭 debug 自动日志 pane（默认开启）
   - `RCCB_DEBUG_WATCH_PROVIDER=<provider>`：指定 debug 日志 pane 跟踪的 provider；默认全局跟踪全部 provider，设为 `all` 或留空也是全局模式
   - `RCCB_DEBUG_WATCH_PANE_PERCENT=<10-80>`：debug 日志 pane 占比（默认 25）
   - `RCCB_ORCHESTRATOR_STRICT=0`：关闭编排者 strict mode（默认开启，且仅在存在执行者时生效）
   - `RCCB_ORCHESTRATOR_CALLBACK_MAX_CHARS=<N>`：限制回注给编排者的结果长度（默认 12000）

静默后台消费排查：

```bash
rccb --project-dir . inbox --instance default --orchestrator claude --limit 20
rccb --project-dir . inbox --instance default --req-id <req_id> --kind result --as-json
```

provider CLI 启动命令可覆盖：

- `RCCB_CLAUDE_START_CMD`
- `RCCB_CODEX_START_CMD`
- `RCCB_GEMINI_START_CMD`
- `RCCB_OPENCODE_START_CMD`
- `RCCB_DROID_START_CMD`

WezTerm CLI 可覆盖：

- `RCCB_WEZTERM_BIN`（默认 `wezterm`）

一键 debug（快捷启动同样生效）：

```bash
RCCB_DEBUG=1 rccb claude gemini opencode
```

编排者 strict mode 下推荐委派格式：

```bash
rccb --project-dir . ask --instance default --provider codex --caller claude "实现 xxx 并自测"
```

当 `caller` 等于当前编排者 provider 时，执行者完成后最终结果会自动后台回注到编排者 pane，供其继续思考和编排。

默认职责：

- `opencode`：编码者
- `gemini`：调研者
- `droid`：文档记录者
- `codex`：代码审计者

调研规则：

- 涉及外部事实、网页资料、版本信息或时间敏感内容时，优先先派给 `gemini`
- `gemini` 默认至少做两轮调研与交叉验证
- 会影响实现或结论的调研结果，必须继续派给 `codex` 复核
- 同步 `ask` 超时后，不要立刻重派，优先使用 `watch --req-id ...` 查看真实状态

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

`rccb start` 会启动项目内 daemon，使用 `rccb` 自身实现的通信协议：

- `ask.ping`
- `ask.request`
- `ask.event`（`start|delta|done|error`，仅流式请求）
- `ask.shutdown`
- `ask.cancel`
- `ask.response`
- `ask.debug`（调试开关）
- `ask.subscribe`（实时事件订阅，`watch` 默认优先使用）

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

# 异步提交（立即返回 req_id，后台执行）
rccb --project-dir . ask \
  --instance team-a \
  --provider codex \
  --caller claude \
  --async \
  --req-id req-async-1 \
  "后台执行这个任务"

# 停止 daemon（优先 graceful shutdown）
rccb --project-dir . stop --instance team-a

# 取消执行中的请求（按 req_id）
rccb --project-dir . cancel --instance team-a --req-id req-123

# 查看任务与 req_id（便于取消/排障）
rccb --project-dir . tasks --instance team-a --limit 20

# 查看 mounted 状态（session exists && daemon online）
rccb --project-dir . mounted --instance team-a --as-json

# 实时观察某个 req_id 的状态变化（异步任务推荐）
rccb --project-dir . watch --instance team-a --req-id req-123

# 按 provider 追踪最新任务（无需手动传 req_id）
rccb --project-dir . watch --instance team-a --provider opencode --with-provider-log

# 常驻追踪（当前任务结束后继续等待下一条）
rccb --project-dir . watch --instance team-a --provider opencode --with-provider-log --follow

# --follow + --provider 默认不超时（tail 风格）
# watch 默认优先走实时总线；断线会按 seq 自动续读；失败时自动回退轮询

# 观察状态 + 关联日志（便于深度排障）
rccb --project-dir . watch \
  --instance team-a \
  --req-id req-123 \
  --with-provider-log \
  --with-debug-log
```

兼容旧习惯后端指令（统一到 `rccb`）：

```bash
# ask
rccb cask "..."
rccb gask "..."
rccb oask "..."
rccb lask "..."
rccb dask "..."

# ping
rccb cping
rccb gping
rccb oping
rccb lping
rccb dping

# pend（读取 default 实例下该 provider 最近任务回复）
rccb cpend
rccb gpend
rccb opend
rccb lpend
rccb dpend
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

watch 相关环境变量：

- `RCCB_WATCH_BUS=0`：关闭实时总线，强制使用旧轮询模式（默认开启）
- `RCCB_EVENT_BUFFER_SIZE=<64-20000>`：事件总线缓冲区大小（默认 2048）
- `RCCB_WATCH_MAX_LOG_LINES=<N>`：轮询模式下每次刷新最多展示 N 行日志（默认 10）

`debug.log` 会记录完整协议收发（`ask.*`）、流式事件和 worker 生命周期，便于复盘与问题反馈。
当 daemon 不在线时，`debug on/off` 会写入实例状态，并在下次 `start` 自动继承。

## Provider 执行适配

支持三种执行模式（`RCCB_EXEC_MODE`）：

1. `native`（默认，推荐）
   - 纯 Rust 直连 provider 进程，不依赖外部桥接入口。
   - 按 provider 查找本地二进制：`claude/codex/gemini/opencode/droid`
   - 优先支持项目内相对绑定：
     - `<project>/.rccb/bin/<provider>`
     - `<project>/bin/<provider>`
   - 支持项目级 profile：
     - `<project>/.rccb/providers/<provider>.json`
     - 字段：`cmd`、`args`、`timeout_s`、`quiet`、`no_wrap`、`env`
   - 原生命令覆盖：
     - `RCCB_<PROVIDER>_NATIVE_CMD`
     - `RCCB_NATIVE_BIN_DIR`
   - 原生参数覆盖：
     - `RCCB_<PROVIDER>_NATIVE_ARGS`
     - `RCCB_NATIVE_ARGS`
   - 默认原生参数（未配置 args 时）：
     - `codex`: `exec`（消息走 stdin）
     - `gemini`: `--prompt {message}`
     - `opencode`: `run {message}`
     - `claude`: `--print {message}`
   - 默认不做 prompt 包装；可显式开启：
     - `RCCB_NATIVE_WRAP=1`
     - `RCCB_<PROVIDER>_NATIVE_WRAP=1`
   - 可关闭包装（对齐 profile）：
     - `RCCB_NATIVE_NO_WRAP=1`
     - `RCCB_<PROVIDER>_NATIVE_NO_WRAP=1`
   - 成功判定：`exit_code=0` 即视为 `completed`（`RCCB_DONE` 仅作为附加标记）。
2. `bridge`（外部 launcher 模式，可选）
   - 通过外部 launcher 启动 provider 包装命令
   - 继承外部 launcher 的会话路由能力，支持 **wezterm / tmux**
   - 命令覆盖：
     - `RCCB_CODEX_CMD`
     - `RCCB_GEMINI_CMD`
     - `RCCB_OPENCODE_CMD`
     - `RCCB_DROID_CMD`
     - `RCCB_CLAUDE_CMD`
   - 外部 launcher 路径：
     - `RCCB_BRIDGE_BIN_DIR`
     - `RCCB_BRIDGE_ROOT`
   - 原生执行策略覆盖：
     - `RCCB_NATIVE_TIMEOUT_S` / `RCCB_<PROVIDER>_NATIVE_TIMEOUT_S`
     - `RCCB_NATIVE_QUIET` / `RCCB_<PROVIDER>_NATIVE_QUIET`
     - `RCCB_NATIVE_STDIN` / `RCCB_<PROVIDER>_NATIVE_STDIN`
   - `args` 支持模板变量：
     - `{req_id}`、`{caller}`、`{provider}`、`{timeout_s}`、`{work_dir}`、`{message}`
   - `env` 值同样支持模板变量：
     - `{req_id}`、`{caller}`、`{provider}`、`{timeout_s}`、`{work_dir}`、`{message}`

   生效优先级（从高到低）：
   - 命令：`RCCB_<PROVIDER>_NATIVE_CMD` > profile `cmd` > `RCCB_NATIVE_BIN_DIR` > 项目 `.rccb/bin` > 项目 `bin` > `PATH`
   - 参数：`RCCB_<PROVIDER>_NATIVE_ARGS` > `RCCB_NATIVE_ARGS` > profile `args`
   - timeout：`RCCB_<PROVIDER>_NATIVE_TIMEOUT_S` > `RCCB_NATIVE_TIMEOUT_S` > profile `timeout_s` > request `timeout_s`
   - quiet：`RCCB_<PROVIDER>_NATIVE_QUIET` > `RCCB_NATIVE_QUIET` > profile `quiet` > request `quiet`
   - no-wrap：`RCCB_<PROVIDER>_NATIVE_NO_WRAP` > `RCCB_NATIVE_NO_WRAP` > profile `no_wrap`
3. `stub`（仅联调）
   - 仅用于通信链路调试，不用于真实 provider 执行

示例：

```bash
# 默认（native）
export RCCB_EXEC_MODE=native
export RCCB_CODEX_NATIVE_CMD=/usr/local/bin/codex
export RCCB_CODEX_NATIVE_ARGS='--model gpt-5-codex'

# 项目级 profile（推荐用于多项目差异配置）
cat > ./.rccb/providers/codex.json <<'JSON'
{
  "cmd": "./.rccb/bin/codex",
  "args": ["--request-id", "{req_id}"],
  "timeout_s": 300,
  "quiet": false,
  "no_wrap": false,
  "env": {
    "RCCB_TASK_ID": "{req_id}",
    "RCCB_TASK_CALLER": "{caller}"
  }
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
- `./.rccb/tasks/<instance>/artifacts/`：按 `req_id` 落盘的请求/结果交换文件
- `./.rccb/tmp/`：provider 临时目录
- `./.rccb/logs/`：daemon/provider 日志

清理规则：

- 运行时临时目录会在对应流程结束后优先清理
- `./.rccb/` 下超过 30 天未更新的旧文件会在后续命令启动时自动清理

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

额外完成态变量：

- `RCCB_CALLER`
- `RCCB_REQ_ID`
- `RCCB_DONE_SEEN`
- `RCCB_COMPLETION_STATUS`

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
4. Provider 执行三模式（`bridge`/`native`/`stub`）
5. 调试开关与完整调试日志
6. IM 通道
7. 任务实时观察（watch）

下一阶段：

1. provider-specific native adapter 深化（逐 provider 能力对齐）
2. completion hook 与回调兼容增强
3. 管理面（mail/web）迁移

详见 `docs/functional-spec.md`、`docs/rewrite-roadmap.md`。

# RCCB 当前路线图

本文档不再描述“从零开始的抽象重构愿景”，而是直接对齐当前项目的真实状态，方便后续继续开发时快速接力。

## 当前基线

1. 已发布版本：`v0.1.0`
2. 发布定位：开发预览版，不是正式稳定版
3. 当前开发分支：`codex/claude-subagent-orchestration`
4. 当前优先方向：文档对齐、子代理编排、静默后台消费、真实交付文件区分、provider 适配收口

## 已完成

### 1. 项目级运行时

1. 统一使用项目内 `.rccb/` 目录承载运行态、任务、日志与临时文件
2. 启动、停止、恢复、清理都已围绕项目目录工作
3. 超过 30 天的旧文件会在后续命令启动时自动清理

### 2. 核心通信链路

1. `ask.request / ask.response` 已由 Rust daemon 承担
2. `ask.event` 流式事件和 `ask.subscribe` 实时总线已接入
3. `watch` 默认优先走实时总线，失败再回退轮询
4. `debug on/off` 可动态切换，debug pane 只在显式开启时出现

### 3. 编排与 pane 运行

1. 快捷启动 `rccb claude opencode gemini codex droid` 已可直接拉起 pane 编排
2. 编排者 strict mode 已接入，默认限制编排者只做思考、拆解、委派、验收、汇总
3. 普通 provider pane 不显示旁路日志，旁路状态只保留在 debug pane
4. 静默后台消费已成为默认路径，避免把执行过程持续刷进编排者 pane

### 4. 项目级规则与 bootstrap

1. `rccb init` 已支持按当前真实 provider 子集生成规则、包装脚本和技能
2. 普通模式仅补缺失文件，debug 模式会刷新 RCCB 托管模板
3. Claude、Codex、Gemini、Droid 的项目级规则文件已统一走中文提示与 RCCB 术语

### 5. Claude 子代理编排

1. `claude` 编排者已生成项目级 `delegate-coder` / `delegate-researcher` / `delegate-auditor` / `delegate-scribe`
2. 已生成 `rccb-parallel` 项目级命令模板，鼓励使用子代理做上下文隔离式异步派单
3. 当前默认职责分工已固化：
   - `opencode`：编码者
   - `gemini`：调研者
   - `droid`：文档记录者
   - `codex`：代码审计者

### 6. 文档交付归一化

1. 文档任务不再把 `.reply.md` 视为最终交付文件
2. `droid` 已补充“保存文件路径 + 摘要”的交付约束
3. 当 `droid` 直接返回正文时，daemon 会自动把正文落盘到 `./temp/rccb-docs/`
4. 同时 `.reply.md` 会被改写成交付索引与摘要，避免编排者误判
5. 底层已写入 `delivery_file` 字段，用来区分真实交付文件和通信工件

## 进行中

### 1. 子代理 fanout / fan-in

当前 Claude 子代理已能独立派单，但主编排者还没有原生的多任务聚合视图：

1. 不会自动汇总多个子代理返回的 `req_id`
2. 不会自动按任务组收敛结果
3. 不会自动生成“哪些任务仍在执行、哪些已完成”的父级摘要

### 2. provider 真并行

目前同一 provider 仍是串行 worker 队列：

1. 多个任务打到同一个执行者时仍会排队
2. 还没有 provider 内部并发池或更细粒度的调度策略
3. 长任务会放大排队等待感

### 3. 静默消费的人机界面

后台消费链路本身已经存在，但 CLI 展示层还没完全收口：

1. `inbox` 已能看到后台 notice
2. `watch` 已能看运行中真实状态
3. `delivery_file` 还没有在 `inbox` / `tasks` / `watch` 里被稳定突出展示

### 4. 多 provider 原生适配收口

当前 `gemini`、`opencode`、`droid`、`codex` 都已经有可用链路，但仍存在边界差异：

1. 长任务与超时后的体验不完全一致
2. pane UI 输出风格仍有差异
3. 个别 provider 在工具调用、输入提交、回传标记上的稳定性仍需继续磨平

## 下一阶段优先项

建议后续继续开发时按下面顺序推进：

1. 为 Claude 子代理补 fanout/fan-in 聚合层
2. 把 `delivery_file` 明确展示进 `inbox` / `tasks` / `watch`
3. 为文档任务增加“是否创建项目级目录”的显式交互或配置项
4. 继续收口 `gemini` / `opencode` / `droid` / `codex` 的原生执行差异
5. 评估同 provider 并行池是否值得引入，以及如何避免状态竞争
6. 补全跨平台测试矩阵和发布验证

## 已知缺口

这些点不是“忘了做”，而是当前明确仍未完成：

1. 编排者对子代理并行任务的统一收敛体验还不完整
2. 同 provider 真并行尚未实现
3. 文档真实交付文件和 `.reply.md` 的差异，底层已经处理，但 CLI 展示层还不够醒目
4. 文档目录确认仍依赖规则与提示词，不是命令级能力
5. Claude 是否总能稳定优先选择子代理，仍会受上游 CLI 行为影响
6. Windows 和更完整的平台发布矩阵仍未补齐

## 文档维护原则

后续如果实现状态变化，请优先同步以下文件：

1. `README.md`：面向使用者的当前能力与启动方式
2. `docs/functional-spec.md`：面向实现的行为基线
3. `CHANGELOG.md`：版本差异与已知缺口

这样可以避免“README 看起来已经完成，但规格和路线图还停在旧时代”的情况再次出现。

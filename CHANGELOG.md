# Changelog

All notable changes to this project will be documented in this file.

## Unreleased

当前开发分支：`codex/claude-subagent-orchestration`

### Added

- `claude` 编排者项目级 `delegate-coder` / `delegate-researcher` / `delegate-auditor` / `delegate-scribe` 子代理模板
- `rccb-parallel` 项目级命令模板，约束 Claude 优先通过子代理做异步派单
- `droid` 文档结果的程序级归一化：当执行者返回整篇正文时，daemon 会自动落盘到 `./temp/rccb-docs/` 并把 `.reply.md` 改写成交付索引与摘要
- `delivery_file` 工件字段，用于区分“真实交付文件”和“通信 reply 工件”

### Changed

- `README.md`、`docs/functional-spec.md`、`docs/rewrite-roadmap.md` 已对齐为当前开发状态，并补充未完成开发项
- 文档任务默认策略从“reply 作为主要交付展示”调整为“真实文件交付优先，reply 只保留索引与摘要”
- `droid` 的底层 prompt 包装新增文档交付约束，默认要求 `saved_files + 摘要`

### Known Gaps

- 子代理派单后的父任务聚合、fanout/fan-in 汇总和同 provider 真并行仍未完成
- `inbox` / `tasks` / `watch` 还没有完整突出 `delivery_file`
- 文档任务的“是否创建项目级目录”仍主要依赖规则和提示词，而非显式交互流程

## v0.1.0 - 2026-03-17

首个开发预览版，重点完成 RCCB 的项目级运行时、pane 编排、静默结果消费和实时状态观察链路。

### Added

- 项目级 `.rccb` 目录模型，统一承载 `run/`、`sessions/`、`tasks/`、`tmp/`、`logs/`
- Rust daemon 与 `ask.*` 协议通信链路
- `ask` 同步、流式、异步、取消、订阅与调试控制能力
- `watch` 实时观察与 provider/debug 日志联动能力
- `inbox` 后台 notice 查看能力
- 项目级 bootstrap：
  - `.rccb/config.example.json`
  - `.rccb/providers/*.example.json`
  - `.rccb/bin/*`
  - `AGENTS.md` / `CLAUDE.md` / `GEMINI.md`
  - provider-specific skills / commands / agents / rules
- tmux / wezterm pane 快捷启动与恢复
- orchestrator strict mode
- `.reply.md` / `.request.md` 工件驱动的任务交换链路
- `.rccb` 30 天保洁策略

### Changed

- 全部术语统一为 `RCCB`，不再兼容旧 `CCB` 关键词
- 普通 provider pane 不再显示旁路日志，旁路日志仅保留在 debug pane
- 静默模式下最终结果优先读取 `.rccb/tasks/<instance>/artifacts/<req_id>.reply.md`
- 快捷启动与恢复逻辑改为按“实际 provider 集合”裁剪生成 wrapper / rules / skills
- 无参 `rccb` 现在会优先恢复默认实例与上次 provider 顺序
- 编排者默认收紧权限，只负责思考、拆解、委派、验收、汇总

### Fixed

- Gemini / Opencode / Droid / Codex 的 pane 执行适配持续补齐
- 结果回传链路从“前台注入优先”调整为“后台消费优先”
- 修复项目级 skill 缺失 YAML frontmatter 导致无法加载的问题
- 修复退出后残留 runtime state 污染下一轮测试的问题
- 修复 debug 状态非预期继承的问题

### Release Assets

- `rccb-v0.1.0-macos-arm64.tar.gz`
- `rccb-v0.1.0-linux-x86_64.tar.gz`
- `SHA256SUMS.txt`

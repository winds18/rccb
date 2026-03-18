# Changelog

All notable changes to this project will be documented in this file.

## v0.1.1 - 2026-03-18

`v0.1.1` 是针对 `v0.1.0` 的热修版本，聚焦 Linux / bash 环境下的启动稳定性，不引入新的编排能力。

### Fixed

- 修复 provider wrapper 写死 `zsh` 导致 Linux / bash 环境启动失败的问题，统一改为 POSIX `sh` 兼容脚本
- 修复 shell 路径回退逻辑：优先使用有效 `SHELL`，否则依次回退到 `/bin/bash`、`/bin/sh` 与 `sh`
- 新增 provider CLI 前置环境检查，缺失依赖时改为中文一次性提示并优雅退出，避免满屏报错
- 新增旧版 `.rccb/bin/*` wrapper 自动刷新机制，升级二进制后会自动修复历史遗留的 zsh 包装脚本

### Tests

- 新增旧版 zsh wrapper 刷新测试
- 新增 provider CLI 缺失时的前置检查测试
- 新增无效 `SHELL` 环境变量下的 shell 回退测试

## v0.1.0 - 2026-03-17

首个正式发布版本，重点完成 RCCB 的项目级运行时、pane 编排、静默结果消费和实时状态观察链路。

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

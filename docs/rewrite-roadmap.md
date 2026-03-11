# RCCB Rust 全量重构路线图

## 阶段 0: 文档冻结

1. 冻结功能范围和兼容边界
2. 冻结 CLI 契约（参数、返回码、输出格式）
3. 冻结状态目录和协议字段

## 阶段 1: 基础运行内核（已完成）

1. 项目级绑定目录 `.rccb/`
2. 多实例锁与状态心跳
3. IM 通道（飞书、Telegram）
4. 单二进制运行

## 阶段 2: 统一 daemon 协议（进行中）

1. `ask.request / ask.response` Rust 化
2. `ask.event` 流式链路（`start/delta/done/error`）
3. `ask.debug` 调试开关与完整日志链路
4. 超时、取消、重试策略 Rust 化

## 阶段 3: Provider Adapter 迁移（进行中）

1. 已接入 CCB wrapper adapter（`lask/cask/gask/oask/dask`）
2. 继续迁移为纯 Rust 原生 adapter（逐 provider）
3. Gemini / OpenCode / Droid / Copilot / Qwen / CodeBuddy
4. 会话恢复与日志跟踪

## 阶段 4: 管理面迁移

1. mail daemon
2. web 管理接口（可选）
3. 监控指标与健康检查

## 阶段 5: 兼容和发布

1. 回归测试矩阵（Linux/macOS/Windows）
2. 兼容旧配置迁移工具
3. 发布单文件二进制（按平台）

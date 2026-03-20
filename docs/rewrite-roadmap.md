# RCCB v0.2.0 主线 TODO

这份文档是当前版本开发的唯一主线清单。

- 目标版本：`v0.2.0`
- 当前已发布稳定版：`v0.1.1`
- 维护规则：
  - 所有新需求、临时插入项、线上 bug、体验问题，先记入本清单，再开始开发。
  - 这份文档与 `README.md` 的“目前状态”必须双向同步；这里是详细版，README 保留摘要版。
  - 只有进入“已收口”且完成必要实测/验证的项，才能视为 `v0.2.0` 候选内容。

## 收口标准

`v0.2.0` 发布前，至少满足以下条件：

1. 编排者稳定
2. 子代理派单稳定
3. 实时状态与超时恢复稳定
4. 项目级规则 / skills / wrappers 行为一致
5. 文档、日志、实际行为三方一致

## 已收口

1. `.rccb/` 目录成为统一项目级运行目录
2. 任务结果工件统一写入 `.rccb/tasks/<instance>/artifacts/<req_id>.reply.md`
3. 调试模式支持启动画面、调试日志 pane、`debug.log` 与运行态重置
4. 快捷启动支持按实际 provider 子集生成项目级 wrapper / rules / skills
5. Linux / bash 启动兼容问题已在 `v0.1.1` 修复
6. 子代理必须通过 RCCB 派单，主编排者直接 `ask` 已被 guard 拦截
7. provider pane 超时时尽量保留部分输出，避免只剩“请求超时”
8. `inbox/status` 已增加 TUI 噪声清洗，减少进度回显污染
9. 人工指定复核执行者已可覆盖默认分工；`复审让 opencode 做，不要找 codex` 已能正确落到 `opencode`
10. `rccb await` 与 `ask --async --await-terminal` 已落地，调研/复核/文档子代理默认改为等待真实终态后再返回
11. `droid` 等执行者的长任务提示回显/占位输出不再被误判为最终 `completed` 结果，并已补单测

## 待收口

### P0 发布阻塞

1. 子代理派单免审批稳定
   - 目标：`delegate-*` 执行 RCCB 派单时，不再因为 `RCCB_*=` 前缀命令触发审批
   - 当前状态：Claude wrapper 已加 `delegate-* -> bypassPermissions` 分支，派单文案也已切到项目级 `./.rccb/bin/rccb-delegate-*`；普通启动现在会自动刷新托管 wrapper，避免老项目继续残留旧派单脚本。剩余工作主要是继续做真实链路实测，确认审批 UI 噪声彻底消失
   - 验收标准：
     - `delegate-researcher`
     - `delegate-auditor`
     - `delegate-coder`
     - `delegate-scribe`
     - 这四类派单都不再弹批准框
     - 不再出现大段 Bash 审批弹窗污染前台 pane

2. 编排者不自己下场执行任务
   - 目标：主编排者只做理解、拆解、委派、验收、汇总
   - 当前状态：规则与 guard 已加，主编排者仍需继续锁死零写权限与受限 Bash 边界
   - 验收标准：
     - 不再改用通用 Agent / WebSearch / Read file 自己完成执行者任务
     - 派单后默认静默等待，不主动刷屏
     - 主编排者不具备任何写文件工具权限

3. 实时状态与超时恢复一致
   - 目标：执行者真实在跑时，编排者不会被误导为“任务已失败”
   - 当前状态：已补上传输异常恢复窗口、timeout 待定窗口、`await-terminal` 终态等待链路，以及长任务 prompt echo / 占位输出去误判；仍需继续做真实编排链路实测
   - 验收标准：
     - 同步 `ask` 超时后，能稳定恢复真实 `req_id` 与任务状态
     - `inbox/watch/reply.md` 三条链路结论一致
     - 编排者不会在执行者仍在运行时错误要求用户裁决
     - 不会再出现 `droid` 等执行者仅回显长任务提示/占位内容，就被误判为 `completed` 并写入 result inbox 的情况

4. 首启与 pane 注入稳定
   - 目标：首次启动时编排提示注入稳定，不需要手动回车
   - 当前状态：已改为“发送后确认，不成功补发 Enter”；tmux 场景已切到启动时直接对当前 tmux server 运行态开启 `mouse on`，并做 `show-options/show-window-options` 回读校验，待继续实测收口
   - 验收标准：
     - `tmux` 稳定
     - `wezterm` 稳定
     - 不重复注入整段提示
     - 通过 `tmux` 启动时，会自动为当前 tmux server 运行态执行 `set-option -g mouse on`
     - 若首轮设置后仍未生效，会补做一次 window 级设置并给出明确错误
     - 不修改用户的 tmux 配置文件

5. 执行结果默认静默回传
   - 目标：执行者完成后，最终结果默认只写入编排者 inbox 与 `.rccb/tasks/<instance>/artifacts/<req_id>.reply.md`，不再默认前台注入编排者 pane
   - 当前状态：daemon 回调默认值与文档已对齐；同步 ask 侧也已改成默认抑制编排者 stdout 结果，只保留 inbox 与 `reply.md` 静默消费
   - 验收标准：
     - 默认情况下，执行者完成不会再向编排者 pane 前台注入 `RCCB_RESULT`
     - 编排者仍可通过 `inbox --latest` 与 `reply.md` 静默消费结果
     - 仅在显式设置 `RCCB_ORCHESTRATOR_RESULT_CALLBACK=1` 时才启用最终结果前台回注
     - 文档、测试、运行行为三方一致

6. Claude 编排者改为“规则/skills 自动加载优先，pane 注入兜底”
   - 目标：首启后优先依赖项目级规则、agents、commands、skills 完成编排行为收敛，不再把 pane 首次提示注入当成关键路径
   - 当前状态：已改为生成 `.claude/rules/rccb-core.md` 与 `.claude/rules/rccb-runtime.md`，并让启动阶段在规则齐全时跳过首启注入；仍需继续实测 tmux / wezterm 首启行为
   - 验收标准：
     - 即使首启注入失败，Claude 仍能按编排者规则工作
     - pane 注入降级为补充提示或 debug 能力，而非主约束来源
     - 项目级规则文件缺失/过旧时，`debug` 模式可自动重建并记录到调试日志

### P1 高优先级

1. 编排者前台进一步去重与降噪
   - 减少“已委派、等待结果”之外的重复描述
   - 长任务前台避免连续状态播报
   - 调研/复核类长任务默认更耐心：无新结论、无异常、未超时前，不主动再次发言，不反复向用户抛“继续等待/稍后查看/是否重试”选择题
   - 当前继续收口方向：对 started/progress 状态做内容级去重，避免执行者重复输出同一条搜索/思考进展时持续刷入编排者 inbox

2. provider-specific native adapter 深化
   - 逐 provider 对齐原生命令、权限与行为差异
   - 优先：`gemini`、`opencode`、`droid`
   - 修复 `droid` 长任务 prompt echo / pane 占位内容被误判为最终结果的问题

3. completion hook 与回调链路增强
   - 完善终态回调和编排者结果消费的一致性

4. 退出/清理进一步静默化
   - pane 清理报错继续收敛
   - 停止流程更安静、更可诊断

5. 子代理阻塞等待的真实链路实测
   - 重点验证 `delegate-researcher` / `delegate-auditor` / `delegate-scribe` 在 tmux / wezterm 下都能稳定等到 `RCCB_AWAIT_DONE`
   - 验证超时、失败、incomplete、取消场景的编排者前台表现是否仍然克制且不乱派单

6. tmux mouse 运行态自动启用
   - 仅在 `tmux` backend 启动 RCCB 时执行
   - 对当前 tmux server 运行态执行 `set-option -g mouse on`
   - 不写入、不改写、不热更新用户的 tmux 配置文件

### P2 后续规划

1. mail daemon / web 管理面
2. 监控指标与健康检查
3. 更完整的跨平台回归矩阵
4. 自动更新体验继续打磨

## 近期变更记录

### 最近已完成

1. 子代理派单已切换到项目级 `./.rccb/bin/rccb-delegate-*` wrapper，由 wrapper 统一注入 RCCB 环境变量并减少审批噪声
2. `delegate-*` 的 Claude wrapper 单独走 `bypassPermissions` 分支
3. 主编排者 strict prompt 与项目级规则进一步加硬
4. pane 注入改为确认式发送
5. 进度回调增加 TUI 噪声清洗
6. 超时结果尽量保留部分输出
7. Claude 编排规则已拆为项目级自动加载入口 + `.claude/rules/rccb-core.md` + `.claude/rules/rccb-runtime.md`
8. 当 Claude 项目级自动加载规则齐全时，首启 pane 注入会自动降级为兜底路径
9. 人工指定复核执行者已通过结构化强标记压过默认 `codex` 分工
10. 同步 `ask` 的传输异常恢复窗口与 timeout 待定窗口已加固，减少执行者仍在运行时的误判失败
11. “最终结果默认静默回传”已继续压实到同步 ask 路径：默认抑制编排者 stdout 结果，仅在显式设置 `RCCB_ORCHESTRATOR_SYNC_STDOUT_RESULT=1` 时才恢复前台打印
12. Claude 项目级 `.claude/settings.local.json` 白名单已和 wrapper 对齐到“读/搜/RCCB 派单”，减少编排者审批噪声并继续保持零写边界
13. 自动生成的 Claude rules/agents/commands 已切换到统一项目入口 `./.rccb/bin/rccb --project-dir .`，减少命令路径漂移导致的审批噪声
14. 普通启动现已支持自动刷新托管的 `.rccb/bin/rccb` 与 `.rccb/bin/rccb-delegate-*` wrapper，老项目无需删除文件或开 debug 也能吃到最新派单修复
15. provider reply 提取已继续加固：若检测到 `RCCB_DONE` 但未能可信定位对应 `RCCB_BEGIN`，会优先判空/不完整，避免把 prompt echo 或任务说明误当最终结果
16. 编排者 progress 状态已增加内容级去重：相同进展短时间内不再重复写入 inbox，仅在内容变化或较长时间后才重发，减少长任务刷屏
17. 编排者 `inbox` 读取结果现已优先采用 `reply_file` 工件内容，再回退事件内联 `reply`，与 `watch/task` 路径的结果优先级保持一致
18. `inbox --latest` 现已在存在终态 `result` 时隐藏同 req/executor 的 `running status`，避免迟到状态把编排者误导成“任务仍在运行”

### 最近新增待办

1. 真实验证子代理无审批派单
2. 真实验证 `tmux / wezterm` 首启注入稳定性
3. 收敛 Claude 子代理 Bash 派单的审批 UI 噪声
4. 强化主编排者“零写权限”硬约束与测试覆盖
5. 继续做真实编排链路实测，确认同步 ask / callback / inbox / reply.md 四条路径在长任务下都不再前台串扰

## 使用约定

后续开发请遵守：

1. 先更新本清单，再改代码
2. 改完代码后，若状态变化，及时把条目从“待收口”移动到“已收口”或补充风险
3. 发版前，以本清单为准生成 `CHANGELOG` / release notes

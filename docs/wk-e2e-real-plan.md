# wk 真实 E2E 测试计划

## 现状与问题

之前的 E2E 测试使用 `fake-claude.py` 模拟 provider，虽然覆盖了 wk 协议本身，但没有验证与真实 coding agent CLI 的集成。需要重写为真实 provider 测试。

## 目标

测试 `wk` 与真实 coding agent CLI 的完整集成：
- Claude Code (`claude` CLI)
- Codex (`codex` CLI)  
- OpenCode (`opencode` CLI) - v1 暂不支持，预留

## 环境要求

### WSL 环境
- WSL Arch Linux（`wk` crate 是 unix-only）
- 需要安装真实的 coding agent CLIs
- 测试用户需要有有效的认证/订阅

### Windows 支持(已完成,2026-10-11)
- `wk` crate 已原生支持 Windows(命名管道 IPC + `CreateProcessW` 守护化),
  见 [wk-windows.md](wk-windows.md)
- Windows 真实 E2E:`target/wk-e2e-real/run-real-windows.sh`
  (claude 21/21、grok 5/5;codex 待其后端连通后补跑)

## 测试策略

### 1. 前置条件检查
- 检查 provider CLI 是否可执行：`type -p claude codex opencode`
- 检查认证状态（各 CLI 可能有不同的 auth 检查命令）
- 设置隔离的测试环境（独立 `WK_HOME`，独立工作目录）

### 2. 测试内容设计

与之前的 fake 测试不同，真实测试需要：

1. **更少的协议细节验证**：
   - 不测试 provider 的精确响应格式
   - 关注功能是否工作，而非协议字段

2. **更多的端到端场景**：
   - 真实的代码查询任务
   - 真实的文件读写任务
   - 真实的权限请求场景
   - 真实的续聊场景

3. **Provider 兼容性**：
   - 每个 provider 可能有不同的特性支持
   - Codex 支持 steer, Claude Code 可能不支持
   - 需要根据 provider 跳过不支持的测试

### 3. 测试组织

```
wk-e2e-real/
├── common.sh           # 共享的测试工具函数
├── check-env.sh        # 前置条件检查
├── test-claude.sh      # Claude Code 专项测试
├── test-codex.sh       # Codex 专项测试
├── test-opencode.sh    # OpenCode 专项测试（预留）
└── run-all.sh          # 总入口
```

### 4. 测试用例设计

#### 基础流程（所有 provider）
- [x] daemon 生命周期
- [ ] 简单查询任务（"列出当前目录的文件"）
- [ ] 读取文件任务
- [ ] 续聊
- [ ] 取消任务
- [ ] timeout 处理

#### 权限流（mode=ask）
- [ ] 拒绝工具调用
- [ ] 批准工具调用
- [ ] auto 模式自动批准

#### 提问流（如果 provider 支持）
- [ ] AskUserQuestion 响应
- [ ] 多选问题

#### 高级特性
- [ ] steer（仅 Codex）
- [ ] background work（如果 provider 支持）
- [ ] 嵌套调用（WK_DEPTH）

### 5. 测试隔离

每个测试运行都应该：
- 使用独立的 `WK_HOME`
- 使用独立的临时工作目录
- 创建临时测试文件
- 测试后清理

### 6. CI 集成考虑

- 真实 provider 测试需要认证，不能在公开 CI 运行
- 可以作为本地验证步骤
- 或者在有密钥的私有 CI runner 上运行

## 实现步骤

1. **Phase 1: 基础设施**
   - [ ] 创建测试目录结构
   - [ ] 实现 `common.sh` 工具函数
   - [ ] 实现 `check-env.sh` 前置检查

2. **Phase 2: Claude Code 测试**
   - [ ] 实现 Claude Code 基础测试
   - [ ] 验证核心流程工作

3. **Phase 3: Codex 测试**
   - [ ] 实现 Codex 基础测试
   - [ ] 测试 Codex 特有功能（steer）

4. **Phase 4: Windows 支持**
   - [x] 实现 `wk` crate 的 Windows 支持(2026-10-11,见 [wk-windows.md](wk-windows.md))
   - [x] 移植测试到 Windows(`run-e2e-windows.sh` / `run-real-windows.sh`)

## 关键差异总结

| 方面 | 之前（fake） | 现在（real） |
|------|------------|------------|
| 测试目标 | wk 协议实现 | 真实集成 |
| Provider | fake-claude.py | 真实 CLI |
| 响应验证 | 精确字符串匹配 | 功能性验证 |
| 执行环境 | 纯本地离线 | 需要网络/认证 |
| CI 适用性 | 完全适用 | 需要密钥 |
| 覆盖面 | 协议边界情况 | 实际使用场景 |

## 下一步行动

1. 在 WSL 中确认 claude 和 codex CLI 可用
2. 实现测试基础设施
3. 开始编写 Claude Code 测试用例

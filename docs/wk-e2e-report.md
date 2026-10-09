# wk 端对端测试报告

日期：2026-10-09
被测实现：`crates/wk`（CLI `wk` + 常驻 daemon `wkd`，同一个二进制）
对照规格：[wk-help.md](wk-help.md) / [wk-spec.md](wk-spec.md)

## 测试环境

- **平台**：WSL Arch（`wk` 仅 unix，Windows 主机不可编译运行）。
- **构建**：`CARGO_TARGET_DIR=$HOME/wk-target cargo build -p wk`（dev profile）。
- **离线 provider**：`fake-claude.py` 按真实协议伪装 `claude` CLI——stream-json stdin/stdout、`can_use_tool` 控制请求（`AskUserQuestion` → question、`Bash` → permission）。daemon 里的 waku-core driver 无感知地把它当真 CLI 拉起，因此整条链路（CLI → unix socket → daemon → driver → provider）都是真实实现，只有 provider 末端是桩。
- **执行身份**：WSL root。原因：joshua 用户的 login-shell PATH 含真 `claude` CLI，而 waku-core 的 `find_executable` 把 login-shell PATH 排在继承 PATH 之前，fake provider 永远抢不到解析权；root 无真 claude，fake 稳定命中。套件开头有 preflight 断言（`bash -lc 'type -p claude'` 必须指向 fake）。

## 测试内容与结果

**18 组 / 94 项，全部通过。**

```
==== wk E2E: 94 passed, 0 failed (18 test groups) ====
```

| 组 | 覆盖内容 | 结果 |
|---|---|---|
| T2 | daemon 生命周期：status 未起=exit 3、首条命令自动拉起、start 幂等、stop 后确认死亡 | 7/7 |
| T1 | 用法错误：无命令 / 缺 `--mode` / PROMPT 与 prompt-file 互斥 / 未知 provider、mode、ctx；amp+ask 拒绝；opencode v1 不支持；help | 10/10 |
| T3 | 基本运行：exit 0、stdout 结果文本、stderr 状态块（task=t1、completed、6 位 ctx id、`next:` 行）、`--model` / `--effort` 透传到 provider 进程参数 | 8/8 |
| T4 | 续聊与只读命令：send 新 turn（t2）、`result`、`result --task t1`、`get --json`、`list`（depth=1）、`list --json` 合法 JSON | 9/9 |
| T5 | `--prompt-file F` 与 `--prompt-file -`（stdin / heredoc） | 4/4 |
| T6 | 提问流：exit 10、`kind=question`、选项摘要、`pending` 全量 JSON、input-required 时 prompt 被拒（13）、自由文本 `--reply`、重复回答（2） | 10/10 |
| T7 | 多子问题：部分 `--answer` 覆盖报错、全量覆盖送达 provider | 5/5 |
| T8 | 权限流：exit 11、`kind=permission`、option ids、deny 生效、auto 模式自批准 | 7/7 |
| T9 | `--timeout`：exit 12、块内 timeout 标记、task 继续跑、`wait` 收尾 | 4/4 |
| T10 | 取消：`wk cancel` exit 130、后台 waiter 同步 130、context 可续聊 | 6/6 |
| T11 | SIGINT 杀 waiter → task canceled（130） | 2/2 |
| T12 | steer：working 时 send 生效、旧 waiter 被顶替（12）、steer 文本到达 provider | 4/4 |
| T13 | `close`：closed 后 send（2）、从 `list` 消失、`--all` 标记 closed | 4/4 |
| T14 | `--on-question fail`（exit 1 + 错误文案）/ `auto`（固定文本代答续跑） | 4/4 |
| T15 | provider 报错 → exit 1、state=failed | 2/2 |
| T16 | 嵌套：超深度（2）、`WK_DEPTH=1` → depth=2、`WK_DEPTH` / `WK_CONTEXT` 注入 provider 环境 | 5/5 |
| T17 | 持久化：daemon 重启后 context / result 完好、自动重拉 | 3/3 |
| T18 | daemon stop 时 working task → failed（`daemon restarted`） | 1/1 |

## 发现并修复的真实 bug（3 个）

1. **`wk daemon stop` 后 daemon 变僵尸**（严重）。
   `serve_connection` 处理 `daemon/shutdown` 响应时先按 `Flow::Disconnect` break，`request_shutdown()` 永远不执行——daemon 不退出、`shutting_down=true` 永久残留，此后每个新 driver 的 pump 线程一进循环就被杀，**所有新任务永久卡在 submitted**。
   定位方式：给 daemon 加毫秒级调试日志（`wkd-debug.log`），看到 `pump: EXIT shutting down` 出现在 `daemon stop` 之后的新请求上。
   修复：`crates/wk/src/daemon.rs` 的 `serve_connection` 中，先判 `method == "daemon/shutdown"` 再判 `Flow::Disconnect`。
2. **`wk result --task T` 的 `--task` 不在 CLI `VALUE_FLAGS`**：被当位置参数吞掉，help 承诺的功能实际不可用。
   修复：`crates/wk/src/main.rs` 的 `VALUE_FLAGS` 加入 `"task"`。
3. **daemon `tasks/result` 忽略 `taskId`**：永远只返回最新 turn 的文本。
   修复：`crates/wk/src/daemon.rs` 按 `--task` 解析 turn 号，新增 `store.rs::load_task_result` 从 `state.db` 读取对应 turn 的 `result_text`。

以上均为源码修复；报告的最终结果在去除全部调试代码后的干净二进制上得出。

## 产物与复跑

- 套件：仓库内 `target/wk-e2e/run-e2e.sh`、桩 `target/wk-e2e/fake-claude.py`（执行前同步到 WSL `/root/wk-e2e/`，并 `sed -i 's/\r$//'` 去 CRLF）。
- 复跑命令（WSL Arch）：

  ```console
  # 构建（joshua 身份，root 无 rustup 默认 toolchain）
  $ cd /mnt/e/CodingSpace/waku/waku
  $ CARGO_TARGET_DIR=$HOME/wk-target cargo build -p wk

  # 执行（root，避免 login-shell PATH 抢占 fake provider）
  $ bash /root/wk-e2e/run-e2e.sh
  ```

- 完整逐项输出：WSL `/root/wk-e2e-final.txt`。

## 已知边界

- `--timeout` 到期路径只输出 stderr 状态块，`--json` 模式下 stdout 没有 JSON 对象（help 文案与实现有出入，未改行为，可按需补）。
- 嵌套 agent 真实回环（provider 内部再调 `wk`）只验证了环境变量注入与 depth 计算，未跑真 agent 回环。
- 真 provider（真实 `claude` / `codex` CLI）不在套件内——本套件验证的是 wk 自身的契约，不验证 provider 适配层对真 CLI 的兼容性。
- 后台任务的 ctx 发现依赖 `wk list --json` 按 `createdAt` 轮询：文本列表是 HashMap 序、且 context 创建有延迟，脚本用 `wait_state working N` 门控消除竞态。

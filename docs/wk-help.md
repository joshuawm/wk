# wk — 使用手册

`wk` 让任意 agent harness（[CC]、Codex、Cursor…）只用 bash 就能调用本地 coding agent：
启动、续聊、回答提问、审批权限、取消。状态全部由常驻 daemon `wkd` 持有，`wk` 本身无状态。
详细设计见 [wk-spec.md](wk-spec.md)。

```console
$ wk -h
usage:
  wk run <provider> [--cwd DIR] [--model M] [--effort E] --mode MODE
        [--on-question yield|fail|auto] [--prompt-file F|-] [PROMPT]
        [--owner PID] [--timeout SECS] [--json]
  wk send <ctx> [--prompt-file F|-] [PROMPT]
  wk send <ctx> --reply <req> [VALUE] [--answer QID=VALUE ...]
  wk wait <ctx> [--timeout SECS] [--json]
  wk get <ctx> [--json]
  wk result <ctx> [--task T]
  wk pending <ctx> [--json]
  wk cancel <ctx> [--json]
  wk close <ctx>
  wk list [--all] [--json]
  wk daemon run|start|stop|status

global: --json prints the TaskStatus JSON object instead of text output;
--timeout SECS stops waiting at the deadline and exits 12 while the task
keeps running.
```

## 30 秒上手

在 background shell（或另一个终端）里跑：

```console
$ wk run codex --mode auto "重构 src/api.rs，把错误处理统一成 thiserror"
（阻塞；provider 干活时 stdout 静默）
[wk] ctx=c7f3a1 task=t1 state=completed
[wk] next: wk send c7f3a1 "<follow-up>"
$ echo "exit=$?"        # exit=0
```

- daemon 没起？任何命令都会自动拉起（后台脱离，等 3s）。
- stdout 只在 completed 时输出结果文本；状态块永远走 stderr，最后一行 `next:`
  告诉你下一步该执行什么。
- 退出码即状态：`0` 完成、`1` 失败、`10/11` 等你回答、`12` 还在跑、`130` 已取消。

## 命令

### wk run — 新会话并跑第一个 turn

```console
$ wk run <provider> --mode MODE [PROMPT]
```

| 参数 | 说明 |
|---|---|
| `<provider>` | `claude` `codex` `cursor` `deepseek` `fx` `grok` `kimi` `amp` `pi` `ohmypi`（`opencode` v1 暂不支持） |
| `--mode` | **必填**：`ask` \| `auto-accept-edits` \| `auto` \| `full-access`。不设默认值，避免隐式给 full access |
| `--cwd DIR` | 工作目录，默认当前目录，存储时规范化为绝对路径，创建后不可变 |
| `--model M` / `--effort E` | 透传给 provider |
| `--on-question` | 提问策略：`yield`（默认，停下等你）\| `fail`（直接失败）\| `auto`（固定文本代答，继续干活） |
| `--owner PID` | 保活对象，默认自动探测父进程链上第一个非 shell 祖先 |

PROMPT 与 `--prompt-file F` 互斥；`--prompt-file -` 从 stdin 读，适合 heredoc：

```console
$ wk run claude --mode auto-accept-edits --prompt-file - <<'EOF'
把 README 翻译成英文，保留原有 markdown 结构
EOF
```

### wk send — 续聊、steer、回答

```console
$ wk send <ctx> "<follow-up>"              # 上一个 turn 已结束 → 新 task
$ wk send <ctx> "<改动方向>"               # 正在跑 → steer（provider 需支持），否则 exit 13
$ wk send <ctx> --reply q1 2               # 回答提问：选 2 号选项
$ wk send <ctx> --reply q1 "直接迁移"       # 或任意自由文本 / 精确 label
$ wk send <ctx> --reply p1 allow_once      # 回答权限请求（也接受 1 起的序号）
```

多个子问题用 `--answer`，可重复；`multi_select` 对同一 id 重复传。必须覆盖全部子问题：

```console
$ wk send c7f3a1 --reply q2 --answer scope=backend --answer scope=api
```

| 当前 task 状态 | `send PROMPT` | `send --reply` |
|---|---|---|
| 无 / 终态 | 创建新 task | 错误 `no-pending-request`（exit 2） |
| working / submitted | steer，不支持则 `rejected`（exit 13） | 错误 |
| input-required | `rejected`（先回答或 cancel） | 回答并等待（exit 2 若 `already-answered`） |

### wk wait — 只等不说话

```console
$ wk wait c7f3a1                # 阻塞到下一个停止点；已在停止点则立即返回（幂等）
$ wk wait c7f3a1 --timeout 600  # 10 分钟没到停止点 → exit 12，task 继续跑
```

每个 context 同时只有一个等待者。新的 `wait`/`send` 会让旧的以 exit 12 退出（`superseded`），不影响 task。
杀掉等待者进程 = 取消当前 task（SIGKILL 除外，由 owner 保活兜底）。

### 只读 / 管理命令

```console
$ wk get c7f3a1            # 当前状态，不阻塞
$ wk result c7f3a1         # 打印 result_text（上一 turn 的完整输出）
$ wk pending c7f3a1        # 列出 pending requests 的完整 JSON payload
$ wk cancel c7f3a1         # 取消当前 task（exit 130 对应 canceled）
$ wk close c7f3a1          # 停 driver、标记 closed，不可再 send
$ wk list                  # 活跃 contexts；--all 含已关闭
```

### wk daemon

```console
$ wk daemon status   # 在跑吗
$ wk daemon start    # 后台拉起（一般不用手动，任何命令都会自动拉起）
$ wk daemon stop     # 停所有 driver；working 的 task 标记 failed，input-required 转 dormant
```

## 状态块与退出码

每次等待结束，stderr 末尾输出状态块，`next:` 行是可以直接复制执行的下一步：

```console
[wk] ctx=c7f3a1 task=t3 state=input-required kind=question
[wk] q1: 保留旧 token 格式吗？ 1) 保留，做兼容层 2) 直接迁移 (或自由回答)
[wk] next: wk send c7f3a1 --reply q1 <answer>
```

```console
[wk] ctx=c7f3a1 task=t3 state=completed
[wk] next: wk send c7f3a1 "<follow-up>"
```

单个请求摘要超过 300 字符会截断，并在 `next:` 前多一行 `[wk] full: wk pending c7f3a1`。

| 退出码 | 含义 |
|---|---|
| 0 | completed |
| 1 | failed |
| 2 | 用法错误 / 请求错误（参数、未知 ctx、`already-answered`…） |
| 3 | daemon 不可用或内部错误 |
| 10 | input-required，question |
| 11 | input-required，permission（两类同时存在时也用 11） |
| 12 | 还在 working（`--timeout` 到期或被新等待者顶替） |
| 13 | rejected（steer 被拒等） |
| 130 | canceled |

外层 harness 只需按退出码分支：`10/11` → 读状态块回答；`12` → 重新 `wk wait`；其余 → 读 `result` 或收工。

## 嵌套调用（agent 调 agent）

daemon 会给 provider 进程注入 `WK_DEPTH` / `WK_CONTEXT`。在 agent 里再调 `wk` 时无需任何配置：

```console
# provider 内部（WK_DEPTH 已是 1）：
$ wk run claude --mode auto "写个测试"
# 新 context 的 depth = 2；超过 WK_MAX_DEPTH（默认 3）→ exit 2, depth-exceeded
```

`wk list` 的 `depth=` 列可以区分哪层会话。宿主 harness 与嵌套会话互不继承环境变量，这是有意为之。

## 生命周期与保活

- **owner**：创建 context 时记录调用方的第一个非 shell 祖先进程（pid + 启动时间，防 PID 复用）。
  每次 `send`/`wait` 重新探测并覆盖——harness 重启后新进程自动接管旧 context。
- **owner 死亡**：宽限 60s（期间被接管则取消处理）；到期 → task canceled、driver 停止，context 和 cursor 保留，之后 `send` 可续。
- **dormant**：`input-required` 挂 30min 没人答 → 停 driver 休眠，状态不变但对系统不可见。
  之后回答（`--reply`）会用 cursor 重启 driver，把答案合成为普通 prompt 继续。
- **空闲 driver**：turn 结束后 driver 保留 10min 用于快速续聊，之后停止。
- **daemon 重启**：working/submitted → failed（`daemon restarted`）；input-required → dormant。context 全保留。

以上阈值当前为编译期常量（60s / 30min / 10min），v1 不可配置。

## 环境变量

| 变量 | 说明 |
|---|---|
| `WK_HOME` | 状态目录，默认 `~/.wk`（`state.db`、`wkd.sock`、`wkd.log`、`wkd.lock`），权限 0700 |
| `WK_DEPTH` | 由 daemon 注入 provider 进程，CLI 读取它计算新 context 的 depth |
| `WK_MAX_DEPTH` | daemon 允许的最大嵌套深度，默认 3 |
| `WK_CONTEXT` | daemon 注入 provider 进程的当前 context id |

## 安全

- 只监听 unix socket `$WK_HOME/wkd.sock`（0600），对端 uid 必须与 daemon 一致，无任何网络监听。
- `state.db` 存有 prompt 与输出全文；目录 0700、文件 0600。
- `--mode full-access` 必须显式传入，没有任何默认路径会到达它。
- `wkd.log` 只记 id 和状态变化，不记 prompt 内容。

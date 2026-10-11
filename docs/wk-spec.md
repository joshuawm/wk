# wk: 跨 agent harness 调用接口 SPEC

状态：草案 v1，供实现者使用。凡标注「待核实」的地方，实现前必须先读代码确认。
（面向使用者的说明见 [wk-help.md](wk-help.md)。）

## 1. 目标

让任意 agent harness（Claude Code、Codex、Cursor 等）只用 bash 就能调用任意 coding agent：启动、续聊、回答提问、审批权限、取消，并通过一个 id 维持会话。

- 状态全部由常驻 daemon `wkd` 持有。CLI `wk` 是无状态 client。
- 语义采用 A2A：context = 会话，task = 一个 turn，task 状态沿用 A2A 状态词汇。
- 通知机制：`wk` 进程在到达停止点时退出。harness 的 background shell 会在进程退出时唤醒外层 agent。杀掉这个进程等于取消当前 task。
- 复用 `waku-core::driver` 作为 provider 适配层，v1 不 fork driver 代码。

非目标（v1）：MCP 前端、A2A 网络协议、远程/多机、GUI。

## 2. 代码组织

- 新 crate：`crates/wk`，只产出一个二进制 `wk`。daemon 通过 `wk daemon run` 运行（同一二进制）。
- 依赖：`waku-core`、`waku-protocol`、`rusqlite`、`serde_json`、`libc`。版本与 workspace 现有依赖保持一致，不引入新的异步运行时，沿用 `smol` / 线程 + `crossbeam-channel`。
- 对 `waku-core` 的必要改动（尽量最小）：
  1. `driver::start_local` 目前是 `pub(crate)`。新增一个 `pub fn start(provider, DriverStartOptions, DriverEventSender) -> anyhow::Result<DriverHandle>`，或直接改为 `pub`。
  2. `DriverStartOptions` 增加 `extra_env: Vec<(String, String)>`，各 driver 在 spawn provider 进程时注入。用于 `WK_DEPTH` / `WK_CONTEXT`（见 §9）。
  3. 确认 `event_channel` 及 `DriverEventSender` 可在 crate 外构造，必要时改为 `pub`。
- 待核实：driver 启动是否依赖 Waku app 专有状态（daemon settings、model catalog、computer use、title refresh）。如有，在 `wk` 中传入中性默认值；`computer_use_enabled` 固定为 `false`。
- 待核实：与 Waku 自身 daemon 同时运行时，OpenCode 共享 service 的 adopt 逻辑是否冲突。v1 可以先不支持 OpenCode，记录为已知限制。

## 3. 数据模型

存储：SQLite，路径 `$WK_HOME/state.db`，`WK_HOME` 默认 `~/.wk`，目录权限 0700。

### Context（会话）

| 字段 | 说明 |
|---|---|
| `id` | 6 位 base32 短 id，例如 `c7f3a1`，唯一 |
| `provider` | `ProviderKind`，序列化为 kebab-case：`claude`、`codex`、`cursor`… |
| `cwd` | 绝对路径，创建后不可变（Claude 会话按项目目录存放，resume 依赖 cwd） |
| `model`、`reasoning_effort`、`mode` | 启动参数；`mode` 对应 `RuntimeMode` |
| `cursor` | `ProviderResumeCursor` JSON，收到 `DriverEvent::Connected { provider_cursor }` 时更新 |
| `owner_pid`、`owner_start_time` | 保活对象，见 §7 |
| `depth` | 嵌套深度 |
| `created_at`、`updated_at`、`closed_at` | |

### Task（一个 turn）

| 字段 | 说明 |
|---|---|
| `id` | context 内自增：`t1`、`t2`… |
| `context_id` | |
| `state` | `submitted` / `working` / `input-required` / `completed` / `failed` / `canceled` / `rejected` |
| `dormant` | bool，见 §7.3 |
| `prompt` | 用户输入 |
| `result_text` | 累积的 `TextDelta`，完成时定稿 |
| `error` | 失败原因 |
| `created_at`、`finished_at` | |

### Request（挂起的输入请求）

| 字段 | 说明 |
|---|---|
| `id` | context 内短 id：提问用 `q1`、`q2`…，权限用 `p1`、`p2`… |
| `task_id` | |
| `kind` | `question` / `permission` |
| `driver_request_id` | driver 原始 `request_id`，只在 daemon 内部使用 |
| `payload` | question：`Vec<UserInputQuestion>`；permission：`{title, detail, options: Vec<PermissionOption>}` |
| `answer`、`answered_at` | |

一个 context 同一时间最多只有一个非终态 task。

## 4. 状态机

```
submitted ──► working ──► completed
                │  ▲  ──► failed
                ▼  │  ──► canceled
          input-required
```

- `submitted → working`：driver 发出 `TurnStarted`。
- `working → input-required`：driver 发出 `Permission` 或 `UserInputRequested`，同时创建 Request。
- `input-required → working`：所有 pending Request 都已回答。
- `→ completed / failed`：`TurnFinished { success }`。
- `→ canceled`：`tasks/cancel`、owner 死亡、等待者被信号杀掉（§6.4）。
- `rejected`：只用于 send 被拒绝的情况（§5.3），不会产生 task 记录，只作为响应状态返回。
- 一个 task 可能同时有多个 pending Request（例如并行工具调用的权限请求）。只要还有未回答的请求，状态就保持 `input-required`。

停止点：`input-required`、`completed`、`failed`、`canceled`。

等待语义是电平触发（level-triggered）：`wait` 检查的是当前 task 状态，不是事件队列。如果当前已经处于停止点，立即返回。所以不存在错过通知的问题，重复 `wait` 是幂等的。

`TurnParked` 的语义待核实。如果它只表示 provider 内部挂起而没有对应的请求，就保持 `working`。

## 5. CLI

全局选项：`--json`（输出单个 JSON 对象，替代文本格式）、`--timeout <secs>`（默认无限，到期以 `working` 退出）。

### 5.1 命令

```
wk run <provider> [--cwd DIR] [--model M] [--effort E] --mode MODE
       [--on-question yield|fail|auto] [--prompt-file F | -] [PROMPT]
wk send <ctx> [--prompt-file F | -] [PROMPT]           # 新 task 或 steer
wk send <ctx> --reply <req> [VALUE] [--answer QID=VALUE ...]
wk wait <ctx>                                          # tasks/resubscribe
wk get <ctx>                                           # 当前状态，不阻塞
wk result <ctx> [--task T]                             # 输出 result_text
wk pending <ctx>                                       # 列出 pending requests 完整内容
wk cancel <ctx>
wk close <ctx>                                         # 停止 driver，标记 closed，不可再 send
wk list [--all]
wk daemon run|start|stop|status
```

- `--mode` 在 `run` 上必填：`ask | auto-accept-edits | auto | full-access`，对应 `RuntimeMode`。不设默认值，避免隐式给出 full access。
- `--cwd` 默认为调用方当前目录，存储时规范化为绝对路径。
- PROMPT 与 `--prompt-file` 互斥。`-` 表示从 stdin 读取，支持 heredoc，用来避免 LLM 拼接引号出错。
- `run`、`send`、`wait` 都是等待者：执行动作，然后阻塞到下一个停止点。
- `get`、`result`、`pending`、`list` 不阻塞。

### 5.2 回答格式

- 提问请求只有一个子问题时：`wk send c7f3a1 --reply q1 2`。VALUE 可以是 1 起的选项序号、选项 label（精确匹配）或任意自由文本。
- 多个子问题时：`--answer <questionId>=<value>`，可重复；`multi_select` 的问题对同一个 id 重复传入。必须覆盖全部子问题，否则报用法错误（exit 2）。
- 权限请求：`wk send c7f3a1 --reply p1 <optionId>`，optionId 来自 `PermissionOption.id`，也接受 1 起的序号。
- 翻译成 driver 调用：question → `respond_user_input(driver_request_id, Vec<UserInputAnswer>)`；permission → `respond(driver_request_id, option_id)`。
- 请求已经回答过：返回错误 `already-answered`（exit 2）。

### 5.3 send 在各状态下的行为

| 当前 task 状态 | `send PROMPT` | `send --reply` |
|---|---|---|
| 无 / 终态 | 创建新 task | 错误 `no-pending-request` |
| working | provider `supports_steer()` 时 steer，然后等待；否则 `rejected` | 错误 |
| input-required | `rejected`（提示先回答或 cancel） | 回答并等待 |

steer 的接受与否以 driver 的 `SteerAccepted` / `SteerRejected` 事件为准，被拒绝时返回 `rejected` 并附带 reason。

### 5.4 输出

运行中 stdout 不输出任何内容（background shell 通常只回传尾部几行）。退出时：

- `completed`：stdout 输出 `result_text`，stderr 最后输出状态块。
- 其他停止点：只在 stderr 输出状态块。
- `--json`：stdout 输出一个 JSON 对象，即 §8 的 `TaskStatus`，不输出文本。

状态块格式（每次都以 `next:` 行结尾，告诉调用方下一步要执行的命令）：

```
[wk] ctx=c7f3a1 task=t3 state=input-required kind=question
[wk] q1: 保留旧 token 格式吗？ 1) 保留，做兼容层 2) 直接迁移 (或自由回答)
[wk] next: wk send c7f3a1 --reply q1 <answer>
```

```
[wk] ctx=c7f3a1 task=t3 state=input-required kind=permission
[wk] p1: Run `rm -rf build/`  options: allow_once, allow_always, reject
[wk] next: wk send c7f3a1 --reply p1 <option>
```

```
[wk] ctx=c7f3a1 task=t3 state=completed
[wk] next: wk send c7f3a1 "<follow-up>"
```

单个请求的摘要超过 300 字符时截断，并在 `next:` 前加一行 `[wk] full: wk pending c7f3a1`。

### 5.5 退出码（从状态派生）

| 码 | 含义 |
|---|---|
| 0 | completed |
| 1 | failed |
| 2 | 用法错误 / 请求错误（参数、未知 ctx、already-answered 等） |
| 3 | daemon 不可用或内部错误 |
| 10 | input-required，question |
| 11 | input-required，permission（同时存在两类时用 11） |
| 12 | working（`--timeout` 到期或被新等待者顶替） |
| 13 | rejected |
| 130 | canceled |

## 6. 等待者

### 6.1 唯一等待者

每个 context 同时最多只有一个等待者。新的等待者（`send` 或 `wait`）注册时，daemon 通知旧等待者 `superseded`，旧等待者以 exit 12 退出，状态块写 `state=working superseded`，并且不触发 cancel。

### 6.2 连接

等待者与 daemon 保持一条 socket 连接。daemon 在 task 进入停止点时推送 `TaskStatus` 通知，等待者打印后退出。

### 6.3 timeout

`--timeout` 到期：等待者断开连接，以 exit 12 退出，`next:` 写 `wk wait <ctx>`。不取消 task。

### 6.4 信号

等待者收到 SIGINT / SIGTERM / SIGHUP 时：

- 如果 task 处于 `working` 或 `submitted`：向 daemon 发送 `tasks/cancel`，等待确认（最多 2s），以 exit 130 退出。
- 其他状态：直接退出。

被 superseded 或 timeout 时不取消。连接意外断开（等待者被 SIGKILL）时 daemon 不取消，交给 §7 的 owner 保活兜底。

## 7. 保活与超时

### 7.1 owner

- 创建 context 时，CLI 从自身沿父进程链向上查找，跳过 shell 和包装进程（`sh bash zsh fish dash env timeout nohup wk`，按进程名匹配），第一个非 shell 祖先就是 owner。找不到时（例如直接在终端里运行）用最顶层的 shell。
- `--owner <pid>` 可显式指定。
- 记录 `pid + 进程启动时间`，用来防止 PID 复用。macOS 用 `proc_pidinfo(PROC_PIDTBSDINFO)` 的 `pbi_start_tvsec`/`usec`；Linux 读 `/proc/<pid>/stat` 第 22 字段。
- 每次 `send` / `wait` 都会重新计算调用方的 owner，并覆盖 context 上的 owner。这样 harness 重启后，新的会话可以接管旧 context。

### 7.2 owner 死亡

daemon 每 5s 检查一次：`kill(pid, 0)` 加上启动时间比对。owner 死亡后有 60s 宽限期（可配置），期间如果被新 owner 接管则取消处理。宽限期结束后：

- 非终态 task → `canceled`，调用 driver `cancel()`。
- 停止 driver，释放 provider 进程。
- context 和 cursor 保留，之后可以 `send` 恢复。

### 7.3 dormant（休眠）

task 处于 `input-required` 且 30min（可配置）没有被回答：停止 driver，设置 `dormant = true`，状态保持 `input-required`，对调用方完全不可见。

provider 进程意外退出、daemon 重启时，`input-required` 的 task 也按 dormant 处理。

回答一个 dormant task 时：用 context.cursor 重新启动 driver，把回答合成为普通 prompt 发送：

- question：`Earlier you asked:\n<问题和选项>\nAnswer: <答案>\nContinue the task.`
- permission：allow → `Permission granted for: <title>. Retry it and continue.`；deny → `Permission denied for: <title>. Continue without it.`

之后 `dormant = false`，状态变为 `working`，后续流程不变。

### 7.4 idle driver

task 进入终态后，driver 保留 10min（可配置），用于快速续聊。之后停止 driver。下一次 `send` 用 cursor 重新启动 driver。

### 7.5 daemon 重启

启动时：`working`/`submitted` 的 task → `failed`，error 为 `daemon restarted`；`input-required` → dormant。context 全部保留。

`working` 状态没有等待者是正常的，不需要超时。

## 8. daemon 协议

- 传输：unix socket `$WK_HOME/wkd.sock`，权限 0600。连接建立后用 `getpeereid` 校验对端 uid 必须与 daemon 相同，否则断开。不提供任何网络监听。
- 编码：JSON-RPC 2.0，每行一条消息（NDJSON）。
- 方法名沿用 A2A 风格：

| 方法 | 参数 | 返回 |
|---|---|---|
| `daemon/ping` | – | `{version}` |
| `contexts/create` | provider, cwd, model?, effort?, mode, onQuestion, owner, depth | `{contextId}` |
| `message/send` | contextId, prompt? 或 reply{requestId, value?, answers?}, wait: bool | `TaskStatus`（wait=false 时立即返回） |
| `tasks/resubscribe` | contextId, owner | 订阅；到达停止点时发 `task/status` 通知 |
| `tasks/get` | contextId, taskId? | `TaskStatus` |
| `tasks/cancel` | contextId | `TaskStatus` |
| `tasks/result` | contextId, taskId? | `{text}` |
| `tasks/pending` | contextId | `Request[]` |
| `contexts/close` | contextId | – |
| `contexts/list` | all? | `Context[]` |
| `daemon/shutdown` | – | – |

服务端通知：`task/status`（TaskStatus）、`waiter/superseded`。

`run` 等于 `contexts/create` + `message/send(wait=true)`。

```jsonc
// TaskStatus
{
  "contextId": "c7f3a1",
  "taskId": "t3",
  "state": "input-required",
  "inputKind": "question",            // 仅 input-required 时存在："question" | "permission"
  "pending": [ /* Request，payload 截断后的摘要 */ ],
  "dormant": false,
  "error": null,
  "next": "wk send c7f3a1 --reply q1 <answer>"
}
```

错误码：用 JSON-RPC error.data.code 携带字符串：`unknown-context`、`context-closed`、`no-pending-request`、`already-answered`、`invalid-answer`、`depth-exceeded`、`provider-unavailable`、`rejected`。

## 9. 嵌套与环境

- CLI 读取环境变量 `WK_DEPTH`（缺省 0），新 context 的 `depth = WK_DEPTH + 1`。超过 `max_depth`（默认 3）时返回 `depth-exceeded`。
- daemon 通过 `extra_env` 给 provider 进程注入 `WK_DEPTH=<depth>` 和 `WK_CONTEXT=<id>`。
- provider 由 daemon 启动，不继承调用方 harness 的环境（例如 `CLAUDECODE`），这是有意为之。provider 所需的 PATH 和登录 shell 环境沿用 `waku-core::command_env` 的解析方式（待核实其入口）。

## 10. on-question 策略

- `yield`（默认）：按 §4 进入 `input-required`。
- `fail`：收到提问时 task → `failed`，error 为 `question-not-allowed`，并附上问题内容。
- `auto`：daemon 立即用固定文本回答：每个子问题都回答 `The user is unavailable. Choose the most reasonable option, state your assumption, and continue.`；有选项时同样传这段自由文本（如果 driver 只接受选项 label，则选第一个选项，待逐个 driver 核实）。

权限请求不受该策略影响，由 `--mode` 决定。

## 11. Provider 能力（基于当前代码 grep，实现前需逐个核实）

| Provider | 发出 Permission | 发出 UserInputRequested | 实现 respond_user_input |
|---|---|---|---|
| Claude | 是 | 是 | 是 |
| Codex | 是 | 是 | 是 |
| Cursor / Fx / Grok / Kimi（ACP） | 是 | 是 | 是 |
| DeepSeek | 是 | 是 | 是 |
| OpenCode | 是 | 是 | 是 |
| Amp | 否 | 否 | 否 |
| Pi / OhMyPi | 否 | 否 | 否 |
| Antigravity（agy） | 否 | 否 | 否（`respond` 为空操作） |

Amp / Pi / OhMyPi 在 `--mode ask` 上直接 `invalid-mode`。Antigravity 接受 ask，但映射成 `agy --sandbox`：进程以 SUCCESS 结束时 `response` 经常为空，wk 仍记 `completed`。纯问答请使用 `full-access`（或 `auto` / `auto-accept-edits`），这三档都传 `--dangerously-skip-permissions`。print 模式没有交互式授权通道。

`supports_steer()` 在运行时查询，不要硬编码。Antigravity 的实现返回 `false`：排队的 stdin 消息要等当前 turn 结束，对 wk 来说就是拒绝 steer（exit 13）。

### Antigravity driver

实现在 `waku-core::driver::agy`。一个会话一个长驻 `agy` 进程，stdin/stdout 都是 NDJSON：

- stdin：`{"event":"user","message":{...}}`，一行一个 turn。
- stdout：`init`（带 `conversation_id`）、`step_update`（`user_input` / `agent_response` / `tool`）、`result`（`status` + `response` + `usage`）。

`init` 里的 id 写成 `ProviderResumeCursor::Antigravity`。续跑用 `--conversation <id>`。`rollback()` 返回错误：agy 不接受调用方指定的会话 ID，wk 的 rewind/fork 语义做不到。`ProviderKind::supports_conversation_rollback` 仍把 Antigravity 列在支持集合里，调用时以 driver 的错误为准。

会话发现读 `~/.gemini/antigravity-cli/conversation_summaries.db`（只读，纯文本列：title / preview / workspace_uris / 时间戳）。`provider_session_history` 固定返回空 history：对应 `conversations/<uuid>.db` 的 `steps` 表是无公开 schema 的 protobuf。wk 自己发起的任务继续用自己的 transcript。

模型发现跑 `agy models`，解析 `id<TAB>display`。斜杠命令目录不探测（插件不在项目树里）。

## 12. 安全

- 只监听 unix socket，0600 权限，加 uid 校验。没有任何认证之外的访问通道，不监听 TCP。
- `state.db` 中保存 prompt 和输出，可能包含敏感信息。目录 0700，文件 0600。
- `full-access` 必须由调用方显式传入。
- 日志（`$WK_HOME/wkd.log`）不记录 prompt 全文，只记录 id 和状态变化。

## 13. daemon 生命周期

- 任何 CLI 命令在连接 socket 失败时自动执行 `wk daemon start`：用 `setsid` 脱离会话、双 fork，然后轮询 socket 最多 3s。
- 用 `$WK_HOME/wkd.lock`（flock）防止重复启动。
- 残留的 socket 文件：无法连接且未持有锁时删除后重建。
- `wk daemon stop`：停止所有 driver，非终态 task 按 §7.5 处理后退出。

## 14. 实现阶段

1. 状态机 + SQLite + JSON-RPC + CLI，使用 fake driver（实现 `DriverControl`，可脚本化发出事件）。
2. 接入 Claude 和 Codex，完成端到端测试。
3. owner 保活、dormant、idle driver、daemon 重启恢复。
4. 接入 ACP 系、DeepSeek、Amp、Pi，最后处理 OpenCode。
5. 后续（不在 v1）：MCP 前端（复用同一套 daemon 协议）、把 driver 拆成独立 crate 并让 waku-core 反向依赖。

## 15. 验收

用 fake driver 的自动化测试必须覆盖：

1. `run` → completed，exit 0，stdout 为结果文本，stderr 最后一行以 `[wk] next:` 开头。
2. `run` → question → exit 10；`send --reply` → completed。
3. 同一 task 内两个并行的 permission：回答一个后立即返回 exit 11，并列出剩下的那个。
4. `wait` 在停止点上重复调用，结果一致（幂等）。
5. 第二个 `wait` 让第一个以 exit 12 superseded 退出，且 task 未被取消。
6. SIGTERM 杀掉 working 中的等待者 → task canceled，context 仍可 `send`。
7. `--timeout 1` → exit 12，task 仍为 working。
8. owner 进程退出 + 宽限期 → task canceled，driver 已停止。
9. dormant 后回答 → driver 用 cursor 重启，收到合成的 prompt。
10. daemon 重启后 `get` 返回 §7.5 规定的状态。
11. working 时 `send` 给不支持 steer 的 driver → exit 13。
12. `WK_DEPTH=3` 时 `run` → `depth-exceeded`，exit 2。
13. 其他 uid 连接 socket 被拒绝（可以用单元测试覆盖校验函数）。

真实 provider 的手工验收：在 Claude Code 中用 background bash 跑 `wk run codex`，触发一次提问，由外层回答，完成后再续聊一轮。

# wk on Windows

`wk` 自 2026-10 起原生支持 Windows(`wk.exe`,无需 WSL)。三个真实
provider(claude / codex / grok)都能被驱动;unix 行为零回归(同一套代码
`#[cfg]` 分支)。

## 与 unix 的差异

| 项 | unix | Windows |
| --- | --- | --- |
| IPC | unix socket(`$WK_HOME/wkd.sock`) | 命名管道 `\\.\pipe\wkd-<user>-<hash(WK_HOME)>`,overlapped I/O |
| 同用户校验 | `getpeereid`/`SO_PEERCRED` | 管道默认 DACL(仅同用户可连) |
| 守护化 | double fork + setsid | `CreateProcessW` + `DETACHED_PROCESS\|CREATE_NEW_PROCESS_GROUP` |
| 信号 | `SIGINT`/`SIGTERM` | `SetConsoleCtrlHandler`(Ctrl+C/Close → 同一原子标志) |
| 随机 | `/dev/urandom` | `getrandom` |
| owner start token | `/proc/<pid>/stat` | `GetProcessTimes` FILETIME |
| cwd 规范化 | `canonicalize` | `dunce::canonicalize`(避免 `\\?\` 泄入 db) |

关键实现约束(改代码前先读):

- **daemon spawn 必须 `bInheritHandles=FALSE`**(`process.rs` 里的裸
  `CreateProcessW`)。std `Command` 在有 stdio 重定向时一揽子继承所有可继承
  句柄 —— bash 命令替换 `$( )` 的管道写端会跟着进 daemon,daemon 永远活着,
  `$( )` 永远等不到 EOF,脚本死锁。daemon 因此没有 std 流,诊断走
  `wkd.log`(启动失败也会落日志)。
- **同步命名管道的所有操作在每个 pipe instance 上是串行的** —— 服务端
  "阻塞读等下一请求 + 另一线程写响应" 的模式必须 overlapped I/O
  (`ipc_windows.rs`),否则服务端自己的写会被自己未完成的读卡死。
- **`Path::exists()` 对 `\\.\pipe\` 无效**,存活探测只能 connect。
- **`Write::flush()` 必须是 no-op**:`FlushFileBuffers` 会阻塞等对端排空。
- stop 语义:客户端发完 shutdown 后**轮询等 endpoint 真正消失**
  (`main.rs`),否则紧跟其后的命令会连上半死的 daemon 拿到
  `unknown-context`。daemon 关闭有真实 provider 会话线程时延迟可达秒级。

## 已知行为差异

- **提权不匹配**:管理员终端起的 daemon 与非管理员客户端(反之亦然)互相
  不可见 —— 默认管道 DACL 的固有行为。同一提权级别内一切正常。
- **owner 语义**:owner 链终止于终端宿主(如 `WindowsTerminal.exe`)而非
  shell —— 与 unix "终端会话拥有上下文" 语义等价。
- **Git Bash 的 `timeout` 杀不死原生进程**,E2E harness 用 `taskkill //F`。
- **T11(SIGINT 取消)在 Git Bash 不可自动化**:msys 对原生子进程的信号
  模拟不可靠。fake 套件默认 SKIP(`WK_E2E_T11=1` opt-in)。**待手动验证**:
  Windows Terminal 里对运行中的 `wk run` 按 Ctrl+C,应走 exit 130。

## E2E 测试

```bash
# fake provider 全链路(93 断言,不耗配额)
bash target/wk-e2e/run-e2e-windows.sh

# 真实 provider(耗配额;WK_E2E_PROVIDERS 过滤)
bash target/wk-e2e-real/run-real-windows.sh
WK_E2E_PROVIDERS="claude grok" bash target/wk-e2e-real/run-real-windows.sh
```

harness 依赖:Git Bash、uv 管理的 python(fake-claude.py 与 JSON 解析;
脚本自动经 `uv python find` 解析并前置 PATH)。MSYS 环境里给 PATH 前置的
必须是 POSIX 形式(`/c/...`),混入 `C:/...` 会按冒号把 PATH 切碎。

## 实测结果(2026-10-11)

- fake:93/93(T11 设计性 SKIP)
- 真实 claude:21/21(basic、读文件、daemon 重启持久化、permission
  allow/deny、auto 自批、timeout、cancel、depth、Windows 路径)
- 真实 grok:5/5(basic、cancel)
- 真实 codex:CLI 可用,但测试当天 codex 后端在本机持续
  `Reconnecting... 1/5`(代理环境问题,与 wk 无关;standalone
  `codex exec` 同样复现),待网络恢复后补跑
- unix 回归(WSL):fake 94/94

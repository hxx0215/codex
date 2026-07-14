# codex-sandbox-server

`codex-sandbox-server` 是一个 Linux-only 的长期驻留子进程服务。它把 Codex
现有的命令沙盒、PTY 和 `command/exec` 控制语义，通过 stdio 和可选的 Unix
Domain Socket（UDS）提供给其他本地服务。

它不包含 TUI、会话、模型调用或用户审批界面。调用方负责展示审批请求。文件系统 sandbox
拒绝仍由调用方决定是否以新请求重试；managed network 的 allowlist miss 会挂起原网络请求，
调用方批准后同一条连接继续执行，不会重跑命令。

## 运行模型

- 父进程必须保持子进程 stdin 打开。stdin EOF 是服务的关闭信号。
- stdio 始终启用，使用一行一个 JSON 对象的 JSONL framing。
- 可以同时启用 WebSocket-over-UDS，允许多个并发连接。
- UDS 文件权限为 `0600`，并通过 Linux peer credentials 只接受同 UID 客户端。
- 每个连接必须先调用 `initialize`。
- `processId` 和运行中的命令都属于创建它们的连接；连接断开时对应命令会被终止。
- 服务退出前会等待运行中的命令完成终止和回收。
- 网络审批和 `acceptForSession` 缓存按连接隔离；连接断开、命令终止或父 stdin EOF 会取消
  对应的待审批请求。

服务复用以下上游实现：

- `codex_core::exec::build_exec_request`
- `codex-sandboxing`
- `codex-utils-pty`
- app-server v2 的 `command/exec` 请求、控制请求和输出通知类型

## 编译

在 `codex-rs` 目录下构建服务和 Linux sandbox helper：

```bash
cargo build --release \
  -p codex-sandbox-server \
  -p codex-linux-sandbox
```

仓库提供 Nix flake 时，也可以从仓库根目录执行：

```bash
nix build .#sandbox-server
```

构建结果位于 `result/bin/`，同时包含 `codex-sandbox-server` 和
`codex-linux-sandbox`。在包含尚未加入 Git index 的本地新文件时，可用
`nix build path:.#sandbox-server` 强制按当前工作目录取源。

如果 flake lock 中的 Rust 版本落后于 `codex-rs/rust-toolchain.toml` 或依赖的最低
Rust 版本，可以在不修改 lock file 的情况下临时刷新 rust-overlay：

```bash
nix develop \
  --override-input rust-overlay github:oxalica/rust-overlay \
  --command cargo build \
  --manifest-path codex-rs/Cargo.toml \
  --release \
  -p codex-sandbox-server \
  -p codex-linux-sandbox
```

构建产物位于：

```text
codex-rs/target/release/codex-sandbox-server
codex-rs/target/release/codex-linux-sandbox
```

可以直接组装发布目录：

```bash
mkdir -p dist
install -m 0755 target/release/codex-sandbox-server dist/
install -m 0755 target/release/codex-linux-sandbox dist/
```

建议将两个文件放在同一个发布目录中：

```text
dist/
├── codex-sandbox-server
└── codex-linux-sandbox
```

服务会自动查找与自身同目录的 `codex-linux-sandbox`。也可以通过
`--codex-linux-sandbox-exe` 显式指定 helper。使用 managed sandbox profile 时必须
保证 helper 可用；`{"type":"disabled"}` 不使用外层 filesystem sandbox。

发布前可以用 `ldd` 检查目标 Linux 环境所需的动态库：

```bash
ldd target/release/codex-sandbox-server
ldd target/release/codex-linux-sandbox
```

## 启动参数

```text
codex-sandbox-server \
  --permission-profile-json <JSON> \
  [--cwd <DIR>] \
  [--uds <PATH>] \
  [--codex-linux-sandbox-exe <PATH>] \
  [--network-proxy-config-json <JSON>]
```

| 参数 | 必需 | 说明 |
| --- | --- | --- |
| `--permission-profile-json` | 是 | 默认的完整 `PermissionProfile` JSON。必须作为一个 argv 参数传入。 |
| `--cwd` | 否 | 默认工作目录，也是相对 `command/exec.cwd` 的解析基准。默认是服务进程当前目录。 |
| `--uds` | 否 | 同时监听 WebSocket-over-UDS。相对路径按服务当前目录解析，生产环境建议传绝对路径。路径已存在时启动失败，不会删除或替换已有文件。 |
| `--codex-linux-sandbox-exe` | 否 | `codex-linux-sandbox` 的显式路径。未传时查找服务可执行文件的同目录 sibling。 |
| `--network-proxy-config-json` | 否 | 完整的 `NetworkProxyConfig` JSON。`enabled: true` 时为每个 `command/exec` 启动 managed proxy，并启用 deferred 网络审批。 |

`--help` 和 `--version` 由 Clap 提供。

### PermissionProfile 示例

只读文件系统、禁用网络：

```json
{
  "type": "managed",
  "file_system": {
    "type": "restricted",
    "entries": [
      {
        "path": {
          "type": "special",
          "value": { "kind": "root" }
        },
        "access": "read"
      }
    ]
  },
  "network": "restricted"
}
```

完全禁用 Codex 外层 sandbox：

```json
{ "type": "disabled" }
```

`disabled` 等同于 danger-full-access，应只在调用方已经完成真人审批，或外层运行环境已经
提供可信隔离时使用。

## 协议

协议沿用 app-server 的无 `jsonrpc: "2.0"` JSON-RPC envelope。每个连接首先发送：

```json
{
  "id": 1,
  "method": "initialize",
  "params": {
    "clientInfo": {
      "name": "parent-service",
      "title": null,
      "version": "1.0.0"
    },
    "capabilities": null
  }
}
```

执行命令：

```json
{
  "id": 2,
  "method": "command/exec",
  "params": {
    "command": ["sh", "-lc", "printf 'hello\\n'"]
  }
}
```

普通退出，包括非零退出码，返回 `completed`：

```json
{
  "id": 2,
  "result": {
    "type": "completed",
    "exitCode": 0,
    "stdout": "hello\n",
    "stderr": ""
  }
}
```

只有 Codex 的 sandbox denial heuristic 判定为沙盒拒绝时才返回：

```json
{
  "id": 2,
  "result": {
    "type": "sandboxDenied",
    "exitCode": 1,
    "stdout": "",
    "stderr": "..."
  }
}
```

支持的方法和通知：

- `initialize`
- `command/exec`
- `command/exec/write`
- `command/exec/resize`
- `command/exec/terminate`
- `command/exec/outputDelta`
- `command/exec/requestNetworkApproval`（server → client request）

流式执行、PTY、timeout、output cap 和参数字段以
[`app-server-protocol/src/protocol/v2/command_exec.rs`](../app-server-protocol/src/protocol/v2/command_exec.rs)
为准。

### Managed network 与 deferred 审批

启动示例：

```bash
codex-sandbox-server \
  --permission-profile-json "$PERMISSION_PROFILE" \
  --network-proxy-config-json '{
    "enabled": true,
    "mode": "full",
    "domains": {
      "api.github.com": "allow",
      "169.254.169.254": "deny"
    },
    "allow_local_binding": false
  }'
```

allowlist 内的公开 host 直接通过代理。显式 denylist、loopback、link-local 和 private
destination 仍由上游代理策略硬拒绝；allowlist miss 会向发起 `command/exec` 的同一连接发送：

```json
{
  "id": "network-approval:1",
  "method": "command/exec/requestNetworkApproval",
  "params": {
    "processId": "build-1",
    "host": "example.com",
    "port": 443,
    "protocol": "https",
    "command": ["cargo", "build"],
    "cwd": "/workspace"
  }
}
```

客户端完成人工审核后，用相同 id 返回已有 app-server 审批 decision：

```json
{
  "id": "network-approval:1",
  "result": { "decision": "acceptForSession" }
}
```

- `accept`：只放行当前网络请求。
- `acceptForSession`：当前请求和该连接内相同 host、port、protocol 的后续请求放行。
- `applyNetworkPolicyAmendment`：allow/deny 结果进入该连接的内存 cache；服务不持久化配置。
- `decline` / `cancel` / error response：拒绝当前网络请求。
- `acceptWithExecpolicyAmendment`：不适用于网络审批，按拒绝处理。

stdout 专用于 JSONL 协议，服务日志写 stderr；不要把普通日志写入 stdout。若父服务需要独立日志
通道，可以单独 pipe stderr、交给进程管理器，或重定向到文件，不需要占用 fd 3。

### 文件系统拒绝后的审批重试

服务不保存审批状态。客户端收到 `sandboxDenied` 后应：

1. 把命令、工作目录、拒绝输出和拟放宽的权限展示给真人。
2. 用户拒绝时结束调用。
3. 用户批准时重新发送新的 `command/exec` 请求，并设置已有的 `sandboxPolicy` 字段。

例如用户批准完全访问后：

```json
{
  "id": 3,
  "method": "command/exec",
  "params": {
    "command": ["sh", "-lc", "original command"],
    "sandboxPolicy": { "type": "dangerFullAccess" }
  }
}
```

该重试是一个新的请求和一次新的命令执行，不是对旧进程的继续运行；它与上面的 managed
network deferred 审批是两套不同语义。

## 验证

```bash
just test -p codex-sandbox-server
just fix -p codex-sandbox-server
just fmt
```

涉及 Cargo dependency 或 workspace member 变化时，还要从仓库根目录执行：

```bash
just bazel-lock-update
```

上游同步流程见 [UPSTREAM_SYNC.md](UPSTREAM_SYNC.md)，Deno 父服务示例见
[DENO_INTEGRATION.md](DENO_INTEGRATION.md)。

# codex-sandbox-server

`codex-sandbox-server` 是一个 Linux-only 的长期驻留子进程服务。它把 Codex
现有的命令沙盒、PTY 和 `command/exec` 控制语义，通过 stdio 和可选的 Unix
Domain Socket（UDS）提供给其他本地服务。

它不包含 TUI、会话、模型调用或用户审批界面。调用方负责让模型/业务层声明最小权限，并把
服务发出的审批请求展示给真人。非空 `additionalPermissions` 会在进程创建前挂起；只有真人
返回单次 `accept` 后才会按原 profile 加增量 overlay 并执行一次。managed network 的 allowlist
miss 也会挂起原网络请求，批准后继续同一条连接，不会重跑命令。

## 运行模型

- 父进程必须保持子进程 stdin 打开。stdin EOF 是服务的关闭信号。
- stdio 始终启用，使用一行一个 JSON 对象的 JSONL framing。
- 可以同时启用 WebSocket-over-UDS，允许多个并发连接。
- UDS 文件权限为 `0600`，并通过 Linux peer credentials 只接受同 UID 客户端。
- 每个连接必须先调用 `initialize`。
- `processId` 和运行中的命令都属于创建它们的连接；连接断开时对应命令会被终止。
- 服务退出前会等待运行中的命令完成终止和回收。
- 网络审批和 `acceptForSession` 缓存按连接隔离；额外文件系统权限不缓存。连接断开、命令
  terminate 或父 stdin EOF 会取消对应的待审批请求。

服务复用以下上游实现：

- `codex_core::exec::build_exec_request`
- `codex-sandboxing`
- `codex-utils-pty`
- app-server v2 的 `command/exec` 请求、控制请求和输出通知类型

## 编译与发布

正式发布应从仓库根目录构建 musl static PIE 包：

```bash
nix build .#sandbox-server
```

Linux 上的 `sandbox-server` 默认指向当前 CPU 架构对应的静态包，也可以显式构建
`nix build .#sandbox-server-static`。构建结果同时包含：

```text
result/bin/codex-sandbox-server
result/bin/codex-linux-sandbox
```

两个文件都没有动态加载器、glibc/musl shared library 或 `/nix/store` 运行时依赖。用 `-L`
解引用 Nix result symlink 后即可复制到其他同 CPU 架构的 Linux 机器：

```bash
mkdir -p dist
cp -L result/bin/codex-sandbox-server dist/
cp -L result/bin/codex-linux-sandbox dist/
```

发布目录保持：

```text
dist/
├── codex-sandbox-server
└── codex-linux-sandbox
```

服务会自动查找与自身同目录的 `codex-linux-sandbox`。也可以通过
`--codex-linux-sandbox-exe` 显式指定 helper。使用 managed sandbox profile 时必须
保证 helper 可用；`{"type":"disabled"}` 不使用外层 filesystem sandbox。

发布前验证两个 ELF：

```bash
file dist/codex-sandbox-server dist/codex-linux-sandbox
ldd dist/codex-sandbox-server
ldd dist/codex-linux-sandbox
```

`file` 应显示 `static-pie linked`，`ldd` 应显示 `statically linked`。静态包支持
`x86_64-linux` 和 `aarch64-linux`，产物不能跨 CPU 架构运行。

日常开发仍可用 Cargo 构建本机动态版本，但不要发布 `target/release` 里的 Nix dev-shell
产物；它们可能包含写死的 `/nix/store` interpreter/RUNPATH。

### GitHub Release workflow

仓库提供 `.github/workflows/sandbox-server-release.yml`。该 workflow 通过 GitHub Actions 页面
手动运行，输入不带 `v` 的 `X.Y.Z` 版本号，例如 `0.1.0`。运行时应选择准备发布的 source ref。

workflow 会：

1. 确认 `sandbox-server-vX.Y.Z` tag 和 release 尚不存在。
2. 只在 CI checkout 中把 workspace placeholder version 临时替换成输入版本。
3. 在原生 x86_64 和 aarch64 Linux runner 上执行 Nix musl static build。
4. strip 并验证两个 ELF，随后把 sibling binaries、`LICENSE`、`NOTICE`、`VERSION` 和
   `SOURCE_COMMIT` 打进架构独立的 `tar.gz`。
5. 生成每个 archive 的 `.sha256`、合并的 `SHA256SUMS` 和机器可读的 `manifest.json`。
6. 在当前 GitHub repository 创建 `sandbox-server-vX.Y.Z` tag 和不可覆盖的 Release。

Release asset 名称如下：

```text
codex-sandbox-server-0.1.0-x86_64-unknown-linux-musl.tar.gz
codex-sandbox-server-0.1.0-x86_64-unknown-linux-musl.tar.gz.sha256
codex-sandbox-server-0.1.0-aarch64-unknown-linux-musl.tar.gz
codex-sandbox-server-0.1.0-aarch64-unknown-linux-musl.tar.gz.sha256
SHA256SUMS
manifest.json
```

同一版本不会被重新发布或覆盖。内容有变化时必须递增版本号，以保证已被其他项目 pin 的 URL 和
SHA-256 始终表示相同字节。

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

完整 wire protocol、字段约束、并发状态机和时序示例见 [PROTOCOL.md](PROTOCOL.md)。

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
- `command/exec/requestPermissionsApproval`（server → client request）

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

### AdditionalPermissionProfile 与启动前审批

Linux sandbox 后端无法可靠返回被拒绝的精确路径；`sandboxDenied.stderr` 只用于诊断，服务绝不
从 stderr 猜测或扩大权限。最小权限必须由模型/业务层在第一次请求中预先声明，或者在首次
失败后由模型明确构造第二个请求。真人只负责 approve/decline，不负责替模型选择路径。

例如命令要新建 `state.json`，应增加对其已存在父目录的写权限：

```json
{
  "id": 3,
  "method": "command/exec",
  "params": {
    "command": ["sh", "-lc", "original command"],
    "processId": "update-state",
    "additionalPermissions": {
      "network": null,
      "fileSystem": {
        "read": null,
        "write": null,
        "entries": [{
          "path": {
            "type": "path",
            "path": "/home/alice/.config/my-app"
          },
          "access": "write"
        }]
      }
    }
  }
}
```

服务随后在同一连接发送：

```json
{
  "id": "permissions-approval:1",
  "method": "command/exec/requestPermissionsApproval",
  "params": {
    "processId": "update-state",
    "command": ["sh", "-lc", "original command"],
    "cwd": "/workspace",
    "additionalPermissions": {
      "network": null,
      "fileSystem": {
        "read": null,
        "write": null,
        "entries": [{
          "path": { "type": "path", "path": "/home/alice/.config/my-app" },
          "access": "write"
        }]
      }
    }
  }
}
```

客户端返回 `{ "decision": "accept" }` 才会启动进程。`decline`、`cancel`、error response、断连
以及 `acceptForSession`/policy amendment 都拒绝启动；额外权限没有安全的 session cache 语义，
因此 fail closed。批准后的 overlay 复用 `codex-sandboxing` 的 effective policy 合并，原 profile
中的 deny/read restrictions 会保留。命令只执行一次。

路径应使用绝对路径，不要传 `~`。Linux bubblewrap 后端把 write path 当作可写目录根，并在其下
合成受保护挂载，因此 write entry 应选择已存在的最深父目录；不要把普通文件路径直接作为
write root。兼容的 `sandboxPolicy`
字段仍保留，但新接入应优先使用 `additionalPermissions`，不要把 `workspaceWrite` 或
`dangerFullAccess` 当默认审批方案。

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
[DENO_INTEGRATION.md](DENO_INTEGRATION.md)，完整通信协议见 [PROTOCOL.md](PROTOCOL.md)。

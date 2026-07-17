# codex-sandbox-server 通信协议

本文档描述 `codex-sandbox-server` 当前实现的完整通信协议，包括 stdio、WebSocket-over-UDS、
请求与响应、命令生命周期、流式 I/O，以及两类真人审批。

协议复用 app-server 的 JSON-RPC envelope 和 v2 `command/exec` 类型，但它不是标准 JSON-RPC
2.0：协议不要求也不发送 `"jsonrpc": "2.0"` 字段。本服务还在
`command/exec` 上增加了本地字段 `additionalPermissions`，并返回本服务特有的 tagged outcome。

代码是协议的最终权威来源：

- envelope：`app-server-protocol/src/rpc.rs`
- `command/exec` 基础类型：`app-server-protocol/src/protocol/v2/command_exec.rs`
- 本地扩展和 outcome：`sandbox-server/src/protocol.rs`
- 请求处理：`sandbox-server/src/server.rs`

## 1. 连接与 framing

### 1.1 stdio

stdio 始终启用。

- client → server：向子进程 stdin 写入 UTF-8 JSONL。
- server → client：从子进程 stdout 读取 UTF-8 JSONL。
- 每条消息必须是单个完整 JSON object，并以换行符 `\n` 结束。
- stdout 只用于协议消息。服务日志写 stderr。
- 父进程必须持续读取 stdout，否则 pipe buffer 填满后会产生 backpressure。
- 父进程关闭 stdin 或退出导致 stdin EOF 时，整个服务开始关闭。

示例：

```text
{"id":1,"method":"initialize","params":{"clientInfo":{"name":"parent","title":null,"version":"1.0.0"},"capabilities":null}}\n
{"id":2,"method":"command/exec","params":{"command":["printf","hello"]}}\n
```

无效 JSON 会被记录到 stderr 并忽略，不会返回 JSON-RPC parse error。因此客户端必须自行保证每行
都是完整、有效的消息。

### 1.2 WebSocket-over-UDS

启动时传入 `--uds <PATH>` 后，服务还会监听 Unix Domain Socket。

- UDS 文件模式为 `0600`。
- 服务通过 Linux peer credentials，只接受 effective UID 与服务相同的 peer。
- 连接建立后必须执行 WebSocket handshake。
- client 可以发送 WebSocket text frame，或内容为有效 UTF-8 JSON 的 binary frame。
- server 只发送 WebSocket text frame；一条 frame 对应一条 JSON 消息，不使用 JSONL 换行。
- 每条 UDS 连接都有独立的初始化状态、请求 id 空间、`processId` 空间和审批 cache。

stdio 是服务的父生命周期连接。即使 UDS 仍有客户端，父进程关闭 stdin 也会关闭服务、终止所有
命令并移除本服务创建的 socket inode。

## 2. 消息 envelope

协议可以抽象为以下 ADT：

```ts
type RequestId = number | string;

type RpcRequest = {
  id: RequestId;
  method: string;
  params?: unknown;
  trace?: unknown;
};

type RpcNotification = {
  method: string;
  params?: unknown;
};

type RpcResponse = {
  id: RequestId;
  result: unknown;
};

type RpcError = {
  id: RequestId;
  error: {
    code: number;
    message: string;
    data?: unknown;
  };
};
```

不要添加 `jsonrpc` 字段。当前实现能够解析可选 `trace`，但 sandbox-server 不对它执行额外处理。

client 发起的请求推荐使用递增整数 id。server 发起审批请求时使用字符串 id：

- `permissions-approval:<n>`
- `network-approval:<n>`

client 必须在同一连接上原样回传 server request 的 id。来自其他连接或未知 id 的审批响应会被
忽略。

### 2.1 错误码

| code | 含义 |
| --- | --- |
| `-32600` | 请求状态或操作无效，例如未 initialize、重复 processId、审批拒绝。 |
| `-32601` | method 不存在。 |
| `-32602` | params 字段或字段组合无效。 |
| `-32603` | sandbox 构造、进程创建或内部处理失败。 |

错误响应表示该 RPC 没有正常产出 result。命令正常退出但 exit code 非零，不是 RPC error。

## 3. 初始化

每条连接的第一个 request 必须是 `initialize`。重复初始化返回 `-32600`；初始化前发送其他
request 也返回 `-32600`。

请求：

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

`clientInfo` 必须存在。当前 sandbox-server 不进行额外 capability negotiation。

响应：

```json
{
  "id": 1,
  "result": {
    "userAgent": "codex-sandbox-server/0.0.0",
    "codexHome": "/home/alice/.codex",
    "platformFamily": "unix",
    "platformOs": "linux"
  }
}
```

## 4. 方法总览

| 方向 | method | 消息种类 | 用途 |
| --- | --- | --- | --- |
| client → server | `initialize` | request | 初始化当前连接。 |
| client → server | `command/exec` | request | 创建并执行沙盒命令。 |
| client → server | `command/exec/write` | request | 写入运行中命令的 stdin，或关闭 stdin。 |
| client → server | `command/exec/resize` | request | 调整 PTY 尺寸。 |
| client → server | `command/exec/terminate` | request | 终止待审批或运行中的命令。 |
| server → client | `command/exec/outputDelta` | notification | 输出流字节。 |
| server → client | `command/exec/requestPermissionsApproval` | request | 启动前额外权限审批。 |
| server → client | `command/exec/requestNetworkApproval` | request | 运行中的 deferred 网络审批。 |

client → server notification 当前会被忽略。需要结果或错误的操作必须作为带 id 的 request 发送。

## 5. `command/exec`

### 5.1 请求字段

```ts
type CommandExecParams = {
  command: string[];
  processId?: string | null;
  tty?: boolean;
  streamStdin?: boolean;
  streamStdoutStderr?: boolean;
  outputBytesCap?: number | null;
  disableOutputCap?: boolean;
  disableTimeout?: boolean;
  timeoutMs?: number | null;
  cwd?: string | null;
  env?: Record<string, string | null> | null;
  size?: { rows: number; cols: number } | null;
  sandboxPolicy?: SandboxPolicy | null;
  permissionProfile?: string | null;
  additionalPermissions?: AdditionalPermissionProfile | null;
};
```

字段规则：

| 字段 | 规则 |
| --- | --- |
| `command` | argv 数组，不能为空。服务直接执行 argv，不额外经过 shell；需要 shell 语法时显式传 `sh -c`。 |
| `processId` | 当前连接内的 client-defined id。PTY、流式 stdin/output 和后续控制请求都要求提供。 |
| `tty` | 默认 `false`。启用后隐含 `streamStdin: true` 和 `streamStdoutStderr: true`。 |
| `streamStdin` | 默认 `false`。允许后续 `command/exec/write`。 |
| `streamStdoutStderr` | 默认 `false`。输出通过 `outputDelta` 发送，不再重复写入最终响应。 |
| `outputBytesCap` | 每个 stdout/stderr 流各自的字节上限。不能与 `disableOutputCap` 同时设置。 |
| `disableOutputCap` | 禁用输出截断。调用方必须承担无界输出的内存和传输风险。 |
| `timeoutMs` | 非负整数毫秒。不能与 `disableTimeout` 同时设置。 |
| `disableTimeout` | 完全禁用命令 timeout。 |
| `cwd` | 省略时使用服务启动 cwd；相对路径以服务启动 cwd 为基准解析。 |
| `env` | 合并到服务根据 shell environment policy 生成的环境。字符串覆盖，`null` 删除变量。 |
| `size` | PTY 字符单元尺寸，`rows` 和 `cols` 必须大于零，且只允许在 `tty: true` 时传入。 |
| `sandboxPolicy` | 覆盖本次命令的基础 sandbox policy。 |
| `permissionProfile` | 上游字段，但 sandbox-server 当前明确拒绝；使用启动 profile、`sandboxPolicy` 或 `additionalPermissions`。 |
| `additionalPermissions` | sandbox-server 本地扩展。非空时先向同一连接请求真人审批。 |

`sandboxPolicy` 和 `permissionProfile` 不能同时出现。由于本服务不支持 named
`permissionProfile`，新客户端通常不应发送该字段。

### 5.2 `SandboxPolicy`

支持的 wire shape：

```ts
type SandboxPolicy =
  | { type: "dangerFullAccess" }
  | { type: "readOnly"; networkAccess?: boolean }
  | {
      type: "externalSandbox";
      networkAccess?: "restricted" | "enabled";
    }
  | {
      type: "workspaceWrite";
      writableRoots?: string[];
      networkAccess?: boolean;
      excludeTmpdirEnvVar?: boolean;
      excludeSlashTmp?: boolean;
    };
```

`dangerFullAccess` 会禁用 Codex 外层 filesystem sandbox，不属于最小权限审批。新接入应优先使用
启动时的完整 `PermissionProfile` 加本次 `additionalPermissions` overlay。

### 5.3 Buffered 请求示例

```json
{
  "id": 2,
  "method": "command/exec",
  "params": {
    "command": ["sh", "-c", "printf stdout; printf stderr >&2; exit 7"],
    "timeoutMs": 10000,
    "env": {
      "BUILD_MODE": "release",
      "REMOVE_ME": null
    }
  }
}
```

最终响应是本服务特有的 tagged union：

```ts
type CommandExecOutcome =
  | {
      type: "completed";
      exitCode: number;
      stdout: string;
      stderr: string;
    }
  | {
      type: "sandboxDenied";
      exitCode: number;
      stdout: string;
      stderr: string;
    };
```

普通非零退出仍是 `completed`：

```json
{
  "id": 2,
  "result": {
    "type": "completed",
    "exitCode": 7,
    "stdout": "stdout",
    "stderr": "stderr"
  }
}
```

只有 Codex 的 sandbox denial heuristic 判定为拒绝时，才返回 `sandboxDenied`。该结果不保证包含
被拒绝的精确路径，客户端不得从 stderr 猜测权限。

命令 timeout 的结果使用 exit code `124`，仍根据最终输出和 sandbox 类型进行 outcome 分类。

## 6. 流式输出和 PTY

流式请求必须提供 `processId`：

```json
{
  "id": 3,
  "method": "command/exec",
  "params": {
    "command": ["sh", "-c", "printf hello; printf error >&2"],
    "processId": "build-1",
    "streamStdoutStderr": true
  }
}
```

server 用 notification 发送原始输出字节：

```json
{
  "method": "command/exec/outputDelta",
  "params": {
    "processId": "build-1",
    "stream": "stdout",
    "deltaBase64": "aGVsbG8=",
    "capReached": false
  }
}
```

- `stream` 为 `stdout` 或 `stderr`。
- PTY 模式把终端输出统一放在 `stdout` stream。
- `deltaBase64` 是原始字节，不保证是 UTF-8。
- `capReached: true` 表示该 stream 已达到 `outputBytesCap`，之后的字节不会继续发送或捕获。
- stdout 与 stderr notification 可以相互交错。
- 对同一个 `command/exec`，最终 response 一定在该命令所有 `outputDelta` notification 之后发送。
- 不同命令、审批请求和 notification 之间可以交错，client 必须持续读取并分发消息。

流式 stream 不会重复进入最终 outcome：

```json
{
  "id": 3,
  "result": {
    "type": "completed",
    "exitCode": 0,
    "stdout": "",
    "stderr": ""
  }
}
```

## 7. stdin、resize 与 terminate

### 7.1 `command/exec/write`

```json
{
  "id": 4,
  "method": "command/exec/write",
  "params": {
    "processId": "repl-1",
    "deltaBase64": "aGVsbG8K",
    "closeStdin": false
  }
}
```

`deltaBase64` 和 `closeStdin: true` 至少要有一个。原命令必须启用 `streamStdin` 或 `tty`。

成功响应：

```json
{"id":4,"result":{}}
```

关闭 stdin 可以单独发送：

```json
{
  "id": 5,
  "method": "command/exec/write",
  "params": {
    "processId": "repl-1",
    "deltaBase64": null,
    "closeStdin": true
  }
}
```

### 7.2 `command/exec/resize`

```json
{
  "id": 6,
  "method": "command/exec/resize",
  "params": {
    "processId": "repl-1",
    "size": {"rows": 40, "cols": 120}
  }
}
```

目标命令应为 PTY 命令。成功响应为 `{}`；无效尺寸、非 PTY 或已结束进程返回 error。

### 7.3 `command/exec/terminate`

```json
{
  "id": 7,
  "method": "command/exec/terminate",
  "params": {"processId": "build-1"}
}
```

成功响应：

```json
{"id":7,"result":{}}
```

- 如果命令仍在等待 `additionalPermissions` 审批，terminate 会取消审批且不会 spawn；原始
  `command/exec` request 随后收到 error。
- 如果命令已经运行，terminate 请求终止进程；原始 `command/exec` request 仍会收到最终 outcome。
- terminate response 与原始 command response/error 的相对到达顺序不应被 client 假定。

## 8. `AdditionalPermissionProfile`

### 8.1 Wire shape

```ts
type AdditionalPermissionProfile = {
  network?: {
    enabled?: boolean | null;
  } | null;
  fileSystem?: {
    // Legacy roots. New clients should prefer entries.
    read?: string[] | null;
    write?: string[] | null;
    globScanMaxDepth?: number | null;
    entries?: Array<{
      path:
        | { type: "path"; path: string }
        | { type: "glob_pattern"; pattern: string }
        | {
            type: "special";
            value:
              | { kind: "root" }
              | { kind: "minimal" }
              | { kind: "project_roots"; subpath?: string | null }
              | { kind: "tmpdir" }
              | { kind: "slash_tmp" }
              | {
                  kind: "unknown";
                  path: string;
                  subpath?: string | null;
                };
          };
      access: "read" | "write" | "deny";
    }> | null;
  } | null;
};
```

规则：

- path 必须是绝对路径；不要发送 `~`。
- 新客户端优先使用 `entries`。当 `entries` 存在时，它是 file-system entries 的权威表示。
- glob entry 只支持 `access: "deny"`；用 glob 扩大 read/write 会返回 `-32602`。
- server 会调用 Codex 上游 normalization：规范化路径、去重并移除空 section。
- Linux bubblewrap 把 write path 当作可写目录根。应授权已存在的最深父目录，不要把普通文件
  路径作为 write root。
- overlay 与本次 `sandboxPolicy` 或服务启动时的默认 `PermissionProfile` 合并，不替换基础
  profile；原有 deny/read restrictions 会被保留。

### 8.2 启动前审批时序

client 发送带非空 `additionalPermissions` 的命令：

```json
{
  "id": 8,
  "method": "command/exec",
  "params": {
    "command": ["sh", "-c", "printf updated > /home/alice/.config/app/state"],
    "processId": "update-state",
    "additionalPermissions": {
      "fileSystem": {
        "entries": [
          {
            "path": {"type": "path", "path": "/home/alice/.config/app"},
            "access": "write"
          }
        ]
      }
    }
  }
}
```

server 不会先执行命令，而是向同一连接发送嵌套 request：

```json
{
  "id": "permissions-approval:1",
  "method": "command/exec/requestPermissionsApproval",
  "params": {
    "processId": "update-state",
    "command": ["sh", "-c", "printf updated > /home/alice/.config/app/state"],
    "cwd": "/workspace",
    "additionalPermissions": {
      "network": null,
      "fileSystem": {
        "read": null,
        "write": null,
        "entries": [
          {
            "path": {"type": "path", "path": "/home/alice/.config/app"},
            "access": "write"
          }
        ]
      }
    }
  }
}
```

审批 request 中的 `additionalPermissions` 是 server normalization 后的权威内容，JSON 表示可能
与原始请求不同。client 应展示并审核 server 发出的 profile，而不是复用未规范化的原始对象。

批准：

```json
{
  "id": "permissions-approval:1",
  "result": {"decision": "accept"}
}
```

拒绝：

```json
{
  "id": "permissions-approval:1",
  "result": {"decision": "decline"}
}
```

也可以返回 error：

```json
{
  "id": "permissions-approval:1",
  "error": {
    "code": -32000,
    "message": "human review service unavailable"
  }
}
```

该审批只接受 `decision: "accept"`。以下情况全部 fail closed，命令不会 spawn：

- `acceptForSession`
- `acceptWithExecpolicyAmendment`
- `applyNetworkPolicyAmendment`
- `decline`
- `cancel`
- malformed response 或 error response
- terminate、连接断开或父 stdin EOF

额外权限审批发生在进程创建和命令 expiration 创建之前，因此等待真人审批的时间不计入
`timeoutMs`。批准后命令只执行一次，不会先失败再重跑。

## 9. Managed network deferred 审批

只有服务启动时传入 enabled `--network-proxy-config-json`，并且运行中的请求命中上游 proxy 的
deferred policy 时，才会发送网络审批。

server → client：

```json
{
  "id": "network-approval:1",
  "method": "command/exec/requestNetworkApproval",
  "params": {
    "processId": "download-1",
    "host": "example.com",
    "port": 443,
    "protocol": "https",
    "command": ["curl", "https://example.com/file"],
    "cwd": "/workspace"
  }
}
```

`protocol` 为：

- `http`
- `https`
- `socks5Tcp`
- `socks5Udp`

单次允许：

```json
{
  "id": "network-approval:1",
  "result": {"decision": "accept"}
}
```

当前连接内缓存允许：

```json
{
  "id": "network-approval:1",
  "result": {"decision": "acceptForSession"}
}
```

应用 host policy cache：

```json
{
  "id": "network-approval:1",
  "result": {
    "decision": {
      "applyNetworkPolicyAmendment": {
        "network_policy_amendment": {
          "host": "example.com",
          "action": "allow"
        }
      }
    }
  }
}
```

审批 decision 的 variant 名使用 camelCase，但带 payload 的上游 enum 仍使用
`network_policy_amendment` / `execpolicy_amendment` 这两个 snake_case 成员名；client 不要自行把它们
改成 camelCase。

行为：

- `accept` 只放行当前网络请求。
- `acceptForSession` 缓存同一连接内相同 normalized host、port 和 protocol 的 allow。
- `applyNetworkPolicyAmendment` 缓存同一连接内该请求 key 的 allow/deny。amendment host 必须与
  请求的 normalized host 相同。
- `decline`、`cancel`、error 或 malformed response 拒绝当前网络请求。
- `acceptWithExecpolicyAmendment` 对网络审批无效，按拒绝处理。
- cache 只存在内存中，连接关闭后清空，不修改启动配置。
- 显式 deny、loopback、link-local、private destination 等上游 hard-deny 不会被 deferred 审批
  放宽。

网络审批发生在命令进程运行期间。命令 timeout、terminate 或连接断开会取消 pending 网络请求；
它不会重新执行整个命令。

## 10. 并发和客户端状态机

一个健壮客户端至少需要同时维护：

```ts
type ClientState = {
  pendingClientRequests: Map<number, PendingPromise>;
  activeProcesses: Map<string, ProcessState>;
  pendingServerRequests: Map<string, HumanReviewState>;
};
```

推荐读取循环：

1. 消息有 `id` 和 `method`：server → client request。异步交给审批 handler，读取循环继续。
2. 消息有 `id`、`result` 或 `error`：完成对应 client request。
3. 消息只有 `method`：处理 notification。
4. 未知 response id 可以记录并忽略，不能错误完成其他 Promise。

关键约束：

- 不能在 `await command/exec` 时停止读取 stdout/UDS，否则 server 发出的审批 request 无法被处理，
  双方会死锁。
- server request handler 不能阻塞唯一的读取循环。真人审批应在独立 Promise/task 中等待。
- `processId` 必须在当前连接内保持唯一，直到待审批或运行中的命令完成。
- 不同连接可以复用相同 `processId`，它们不会相互控制。
- 不要假设并发 command response、控制 response、审批 request 和跨 stream notification 的全局
  顺序。
- 对单个流式命令，只能依赖“所有 outputDelta 先于该命令最终 response”。

## 11. 断连与关闭语义

连接关闭时：

- 取消该连接的 pending additional-permissions approval。
- 取消该连接的 pending network approval。
- 清除该连接的 network session cache。
- 终止并回收该连接创建的所有运行中命令。
- 已关闭连接不会再收到最终 response。

父 stdio stdin EOF 还会触发全服务 shutdown：停止 UDS listener、关闭 UDS 连接、终止剩余命令，
等待回收完成后退出。

## 12. 完整时序示例

### 12.1 启动前文件系统审批

```text
client                                      sandbox-server
  |                                               |
  | initialize request                            |
  |---------------------------------------------->|
  |                              initialize result|
  |<----------------------------------------------|
  |                                               |
  | command/exec + additionalPermissions          |
  |---------------------------------------------->|
  |                                               | normalize permissions
  | requestPermissionsApproval                    | process not spawned
  |<----------------------------------------------|
  |                                               |
  | human review                                  |
  |                                               |
  | {decision: accept}                            |
  |---------------------------------------------->|
  |                                               | merge profile, spawn once
  |                                               |
  |                           command final outcome|
  |<----------------------------------------------|
```

### 12.2 运行中的网络审批

```text
client                                      sandbox-server
  |                                               |
  | command/exec                                  |
  |---------------------------------------------->|
  |                                               | spawn command + proxy
  |                                               | command connects to host
  | requestNetworkApproval                        | original connection waits
  |<----------------------------------------------|
  |                                               |
  | {decision: acceptForSession}                  |
  |---------------------------------------------->|
  |                                               | same network request resumes
  | outputDelta notifications                     |
  |<----------------------------------------------|
  |                           command final outcome|
  |<----------------------------------------------|
```

## 13. 兼容性边界

本协议当前是仓库内服务协议，没有独立 version negotiation。客户端升级 server 二进制时，应检查：

- app-server v2 `command/exec` 参数是否变化。
- approval decision 和 `AdditionalPermissionProfile` wire shape 是否变化。
- tagged `CommandExecOutcome` 和 method 名称是否变化。
- `UPSTREAM_SYNC.md` 中列出的上游同步检查项。

父服务应对未知 notification 宽容忽略，但对未知 server request fail closed 并返回 error，避免未经
审核的能力被静默批准。

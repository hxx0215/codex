# Deno 父服务接入示例

示例代码位于 [`examples/deno_parent.ts`](examples/deno_parent.ts)。它演示一个 Deno 服务如何：

1. 将 `codex-sandbox-server` 作为子进程启动。
2. 通过 argv 传入默认 `PermissionProfile`。
3. 保持 stdin 打开，并使用 JSONL 发送并发 JSON-RPC 请求。
4. 在后台持续读取 stdout，将 response 按 id 分发给对应 Promise。
5. 接收 `command/exec/outputDelta` notification。
6. 处理 `command/exec/requestNetworkApproval` server request，并把真人审核结果写回相同 id。
7. 在关闭时主动关闭 stdin，并等待 sandbox server 完整退出。

## 准备发布目录

先按 [README.md](README.md) 编译，然后准备：

```text
dist/
├── codex-sandbox-server
└── codex-linux-sandbox
```

两个可执行文件放在同一目录时，server 会自动找到 Linux sandbox helper。

## 运行示例

从 `codex-rs/sandbox-server` 目录运行：

```bash
CODEX_SANDBOX_SERVER=../target/release/codex-sandbox-server \
deno run --allow-run --allow-env examples/deno_parent.ts
```

如果 helper 不在 server 同目录，可以额外设置：

```bash
CODEX_SANDBOX_SERVER=/opt/codex/bin/codex-sandbox-server \
CODEX_LINUX_SANDBOX=/opt/codex/bin/codex-linux-sandbox \
deno run --allow-run --allow-env examples/deno_parent.ts
```

生产服务通常已经拥有自己的配置和日志系统，可以把示例中的环境变量替换为配置字段。

## stdio framing

每条 stdin 消息必须是一个完整 JSON object，并以 `\n` 结尾：

```text
{"id":1,"method":"initialize","params":{...}}\n
{"id":2,"method":"command/exec","params":{...}}\n
```

stdout 也是一行一个 JSON object。不能把 stdout 当作普通日志；服务日志应走 stderr。

Deno 父服务必须持续读取 stdout。若父服务停止读取且 pipe buffer 被填满，流式通知和最终响应
都会产生 backpressure。

## 并发请求

请求 id 只需要在当前连接内唯一。示例为每个请求递增 id，并维护：

```ts
Map<number, { resolve; reject }>
```

收到带 `id` 的 response/error 后完成对应 Promise。没有 `id`、但带 `method` 的消息是
notification，例如：

```json
{
  "method": "command/exec/outputDelta",
  "params": {
    "processId": "build-1",
    "stream": "stdout",
    "deltaBase64": "aGVsbG8K",
    "capReached": false
  }
}
```

`deltaBase64` 是原始输出字节的 base64，不应先假设它是 UTF-8。需要显示文本时，再由客户端
决定使用严格 UTF-8、replacement decoding 或二进制处理。

server 也会发送带字符串 id 的 request。示例通过 `onRequest` 异步处理它，回调完成后写回
`{id, result}` 或 `{id, error}`。读取 stdout 的循环不能在等待真人审核时暂停，否则其他响应、
通知和并发审批都会被阻塞。

## Managed network 审批

通过 `networkProxyConfig` 传入上游 `NetworkProxyConfig`：

```ts
const client = await SandboxServerClient.start({
  binary,
  cwd: Deno.cwd(),
  permissionProfile,
  networkProxyConfig: {
    enabled: true,
    mode: "full",
    domains: { "api.github.com": "allow" },
    allow_local_binding: false,
  },
  onRequest: async (method, params) => {
    if (method !== "command/exec/requestNetworkApproval") {
      throw new Error(`unsupported request: ${method}`);
    }
    const approved = await reviewService.requestHumanApproval(params);
    return { decision: approved ? "acceptForSession" : "decline" };
  },
});
```

allowlist miss 期间原网络连接保持等待。`accept` 只放行一次，`acceptForSession` 只在当前 stdio
或 UDS 连接内缓存；断连后 cache 清空。Deno 服务若要持久化规则，应在自己的配置层保存，并在
下次启动时重新生成 `networkProxyConfig`。

## 文件系统拒绝后的审批重试

示例导出了 `execWithApproval`。流程是：

1. 先按默认 profile 调用 `command/exec`。
2. 如果返回 `completed`，直接结束。
3. 如果返回 `sandboxDenied`，调用 Deno 服务自己的真人审核回调。
4. 回调批准后，以新的 request id 再次发送命令，并设置更宽松的 `sandboxPolicy`。

示例中的审批回调只是接口。生产服务应把命令、cwd、拒绝信息和权限差异发送到自己的管理
界面，不要在 sandbox server 内增加审批状态。

```ts
const result = await execWithApproval(
  client,
  {
    command: ["sh", "-lc", "touch generated.txt"],
  },
  async (denied) => {
    return await reviewService.requestHumanApproval({
      command: "touch generated.txt",
      stderr: denied.stderr,
      requestedPolicy: "dangerFullAccess",
    });
  },
);
```

批准后的第二次执行是全新的进程。第一次被拒绝的进程不会被恢复。

## 使用 UDS

Deno 作为父进程时通常直接使用 stdio，路径最短，也天然绑定服务生命周期。如果还要让其他
同 UID 进程共享这个 sandbox server，可以启动时追加：

```ts
args.push("--uds", "/run/user/1000/codex-sandbox-server.sock");
```

父 Deno 服务仍需保持 stdin 打开。UDS 客户端必须使用 WebSocket framing，并且每个 UDS
连接都要独立调用 `initialize`。Deno 标准 `WebSocket` API 不能直接连接 Unix socket；需要使用
支持 WebSocket-over-UDS 的库，或基于 `Deno.connect({ transport: "unix" })` 实现客户端 framing。

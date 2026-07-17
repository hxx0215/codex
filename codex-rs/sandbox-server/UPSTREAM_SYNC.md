# 上游 Codex 更新后的同步方法

`codex-sandbox-server` 是独立 crate，没有修改 app-server 的执行实现。因此日常合并上游时，
通常只会在 workspace member 或 lock file 附近产生文本冲突；代价是 app-server 执行语义发生
变化时，需要主动检查并同步到本 crate。

不要把 app-server 文件整体复制覆盖到这里。应当逐项移植协议或行为变化，以保持服务边界和
Linux-only 约束。

## 需要关注的上游文件

### 协议

- `codex-rs/app-server-protocol/src/protocol/v2/command_exec.rs`
- `codex-rs/app-server-protocol/src/protocol/v2/permissions.rs`
- `codex-rs/app-server-protocol/src/rpc.rs`

重点检查：

- `CommandExecParams` 是否新增、删除或重命名字段。
- 本 crate 的 serde flatten wrapper 是否仍能在不修改公共 `CommandExecParams` 的前提下解析
  camelCase `additionalPermissions`。
- app-server v2 `AdditionalPermissionProfile` 与审批 decision/response 的 wire shape 是否变化。
- write、resize、terminate 的参数和响应是否变化。
- `command/exec/outputDelta` 的字段、base64 语义和发送顺序是否变化。
- serde 的 camelCase、tagged union 或 optional/default 规则是否变化。

### app-server 执行语义

- `codex-rs/app-server/src/request_processors/command_exec_processor.rs`
- `codex-rs/app-server/src/command_exec.rs`

重点检查：

- processId 的作用域和重复检查。
- PTY、stdin streaming 和 stdout/stderr streaming 的约束。
- timeout、timeout exit code、output cap 和 drain timeout。
- 输出通知必须先于最终响应的顺序保证。
- 连接断开和服务关闭时的子进程终止逻辑。
- cwd、环境变量覆盖、sandboxPolicy 和 permissionProfile 的解析规则。

### sandbox 与进程 API

- `codex-rs/core/src/exec.rs`
- `codex-rs/core/src/config/network_proxy_spec.rs`
- `codex-rs/core/src/tools/network_approval.rs`
- `codex-rs/network-proxy/src/`
- `codex-rs/sandboxing/src/denial.rs`
- `codex-rs/sandboxing/src/lib.rs`
- `codex-rs/utils/pty/`
- `codex-rs/protocol/src/models.rs`
- `codex-rs/protocol/src/permissions.rs`

重点检查：

- `build_exec_request` 的签名和必须传入的新上下文。
- `NetworkPolicyDecider` 的等待语义、host policy 优先级和 private/local hard-deny 行为。
- `NetworkProxy` builder、`NetworkProxyHandle`、proxy env 和 managed network sandbox context。
- Linux sandbox type 的选择方式。
- `codex-linux-sandbox` helper 的参数和发现方式。
- `is_likely_sandbox_denied` 的输入和判定变化。
- `PermissionProfile` JSON shape 和 materialization 规则。
- `normalize_additional_permissions`、`effective_permission_profile` 以及 deny/read restriction 的
  merge 语义；本服务不得复制一份自己的合并实现。
- `SpawnedProcess`、`ProcessHandle`、resize、close stdin 和 terminate API。

### 静态发布构建

- `flake.nix`
- `codex-rs/default.nix`
- `codex-rs/core/Cargo.toml` 中 musl target 的 vendored OpenSSL 配置

重点检查 `nix build .#sandbox-server` 在 Linux 上仍生成当前架构的 musl static PIE，并且
同时安装 server 与 helper。不要退回直接发布由 Nix dev shell 生成的 `target/release` 动态
ELF。

### transport 行为

- `codex-rs/app-server-transport/src/transport/stdio.rs`
- `codex-rs/app-server-transport/src/transport/websocket.rs`
- `codex-rs/app-server-transport/src/transport/mod.rs`

本服务有自己的精简 transport，但 JSONL、WebSocket message 和异常处理应继续与 app-server
保持一致。UDS 的同 UID 校验、`0600` 和父 stdin 生命周期规则是本服务特有行为，不应被上游
TCP/WebSocket 改动覆盖。

## 推荐同步步骤

假设 `BASE` 是上次完成同步时的上游 commit，`TARGET` 是准备合入的新上游 commit：

```bash
git fetch upstream
BASE=<last-synced-upstream-sha>
TARGET=upstream/main
```

先查看相关区域的提交和 diff：

```bash
git log --oneline "$BASE..$TARGET" -- \
  codex-rs/app-server-protocol/src/protocol/v2/command_exec.rs \
  codex-rs/app-server/src/command_exec.rs \
  codex-rs/app-server/src/request_processors/command_exec_processor.rs \
  codex-rs/core/src/exec.rs \
  codex-rs/sandboxing \
  codex-rs/utils/pty \
  codex-rs/protocol/src/models.rs \
  codex-rs/protocol/src/permissions.rs
```

```bash
git diff "$BASE..$TARGET" -- \
  codex-rs/app-server-protocol/src/protocol/v2/command_exec.rs \
  codex-rs/app-server/src/command_exec.rs \
  codex-rs/app-server/src/request_processors/command_exec_processor.rs \
  codex-rs/core/src/exec.rs \
  codex-rs/sandboxing \
  codex-rs/utils/pty \
  codex-rs/protocol/src/models.rs \
  codex-rs/protocol/src/permissions.rs
```

然后按以下顺序处理：

1. 先让 `codex-sandbox-server` 在新上游上通过编译，处理类型和函数签名变化。
2. 对照 app-server 更新参数校验和进程生命周期语义。
3. 对照 sandboxing 更新拒绝分类；保持本服务的 tagged outcome：
   `completed | sandboxDenied`。
4. 检查启动前 additional permissions 审批仍只接受单次 `accept`，并在批准后通过上游 effective
   policy overlay 执行一次。sandbox denial 后端不能可靠返回 denied path，服务不得从 stderr
   推断授权。
5. 对照 transport 更新 framing 和 notification 顺序；保留 UDS 的同 UID 限制。
6. 更新或增加集成测试，不只做编译修复。
7. 在同步提交或 PR 描述中记录新的上游 commit SHA，作为下一次 `BASE`。

## 必须重新验证的行为

- stdio 未 initialize 时拒绝请求。
- stdio initialize 后可以执行命令。
- 普通非零退出仍是 `completed`。
- sandbox denial 是 `sandboxDenied`。
- streamed output 不会重复进入最终 stdout/stderr。
- write、resize、terminate 仍然按连接和 processId 路由。
- UDS mode 为 `0600`，同 UID 可以连接。
- UDS 断连会终止该连接创建的进程。
- allowlist host 不触发审批；allowlist miss 挂起并在批准后继续同一网络请求。
- denylist/private/local destination 不会被 deferred 审批放宽。
- `acceptForSession` cache 不跨连接，断连和 terminate 会取消 pending approval。
- 非空 `additionalPermissions` 在 spawn 前发送
  `command/exec/requestPermissionsApproval`；Accept 才执行，Decline/Cancel/error/
  AcceptForSession/断连都不执行。
- additional permissions 使用上游 effective profile merge，保留已有 deny/read restrictions；
  不带该字段的 command 行为不变。
- 两个发布 ELF 的 `file` 结果都是 `static-pie linked`，`ldd` 都是 `statically linked`。
- 父进程 stdin EOF 会关闭 UDS、终止全部进程并删除本进程创建的 socket inode。
- 已存在的 UDS path 不会被删除或替换。

## 验证命令

只修改本 crate 时：

```bash
cd codex-rs
just test -p codex-sandbox-server
just fix -p codex-sandbox-server
just fmt
```

如果同步时修改了 `common`、`core` 或 `protocol`，还应执行完整测试：

```bash
cd codex-rs
just test
```

Cargo dependency 或 workspace member 发生变化时：

```bash
just bazel-lock-update
```

最后检查依赖边界：

```bash
cd codex-rs
cargo tree -p codex-sandbox-server
```

本 crate 不应直接依赖 `codex-tui` 或 app-server implementation crate。复用 app-server 的
protocol crate、Codex core sandbox construction 和 PTY crate 是预期行为。

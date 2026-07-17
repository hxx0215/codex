type JsonObject = Record<string, unknown>;

type RpcFailure = {
  code: number;
  message: string;
  data?: unknown;
};

type PendingRequest = {
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
};

export type CommandExecOutcome =
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

export type AdditionalPermissionProfile = {
  network?: { enabled?: boolean | null } | null;
  fileSystem?: {
    read?: string[] | null;
    write?: string[] | null;
    entries?:
      | Array<{
        path:
          | { type: "path"; path: string }
          | { type: "glob_pattern"; pattern: string }
          | { type: "special"; value: JsonObject };
        access: "read" | "write" | "deny";
      }>
      | null;
  } | null;
};

export type ServerRequestHandler = (
  method: string,
  params: unknown,
) => Promise<unknown>;

export class SandboxServerClient {
  readonly #child: Deno.ChildProcess;
  readonly #stdin: WritableStreamDefaultWriter<Uint8Array>;
  readonly #pending = new Map<number, PendingRequest>();
  readonly #encoder = new TextEncoder();
  readonly #onNotification: (method: string, params: unknown) => void;
  readonly #onRequest: ServerRequestHandler;
  #nextId = 1;
  #writeTail: Promise<void> = Promise.resolve();
  #readTask: Promise<void>;
  #closed = false;

  private constructor(
    child: Deno.ChildProcess,
    onNotification: (method: string, params: unknown) => void,
    onRequest: ServerRequestHandler,
  ) {
    this.#child = child;
    this.#stdin = child.stdin.getWriter();
    this.#onNotification = onNotification;
    this.#onRequest = onRequest;
    this.#readTask = this.#readStdout();
  }

  static async start(options: {
    binary: string;
    cwd: string;
    permissionProfile: JsonObject;
    linuxSandboxExe?: string;
    uds?: string;
    networkProxyConfig?: JsonObject;
    onNotification?: (method: string, params: unknown) => void;
    onRequest?: ServerRequestHandler;
  }): Promise<SandboxServerClient> {
    const args = [
      "--permission-profile-json",
      JSON.stringify(options.permissionProfile),
      "--cwd",
      options.cwd,
    ];
    if (options.linuxSandboxExe !== undefined) {
      args.push("--codex-linux-sandbox-exe", options.linuxSandboxExe);
    }
    if (options.uds !== undefined) {
      args.push("--uds", options.uds);
    }
    if (options.networkProxyConfig !== undefined) {
      args.push(
        "--network-proxy-config-json",
        JSON.stringify(options.networkProxyConfig),
      );
    }

    const child = new Deno.Command(options.binary, {
      args,
      cwd: options.cwd,
      stdin: "piped",
      stdout: "piped",
      stderr: "inherit",
    }).spawn();
    const client = new SandboxServerClient(
      child,
      options.onNotification ?? (() => {}),
      options.onRequest ?? (async (method) => {
        throw new Error(`unhandled sandbox server request: ${method}`);
      }),
    );
    await client.request("initialize", {
      clientInfo: {
        name: "deno-parent-service",
        title: "Deno Parent Service",
        version: "1.0.0",
      },
      capabilities: null,
    });
    return client;
  }

  async request<T>(method: string, params: JsonObject): Promise<T> {
    if (this.#closed) {
      throw new Error("sandbox server client is closed");
    }
    const id = this.#nextId++;
    const response = new Promise<T>((resolve, reject) => {
      this.#pending.set(id, {
        resolve: (value) => resolve(value as T),
        reject,
      });
    });
    try {
      await this.#write({ id, method, params });
    } catch (error) {
      this.#pending.delete(id);
      throw error;
    }
    return await response;
  }

  async close(): Promise<Deno.CommandStatus> {
    if (!this.#closed) {
      this.#closed = true;
      await this.#writeTail;
      await this.#stdin.close();
    }
    const status = await this.#child.status;
    await this.#readTask;
    return status;
  }

  async #write(message: JsonObject): Promise<void> {
    const bytes = this.#encoder.encode(`${JSON.stringify(message)}\n`);
    const write = this.#writeTail.then(() => this.#stdin.write(bytes));
    this.#writeTail = write.catch(() => {});
    await write;
  }

  async #readStdout(): Promise<void> {
    const reader = this.#child.stdout.getReader();
    const decoder = new TextDecoder();
    let buffered = "";
    let failure: Error | undefined;
    try {
      while (true) {
        const { value, done } = await reader.read();
        buffered += decoder.decode(value, { stream: !done });
        let newline = buffered.indexOf("\n");
        while (newline >= 0) {
          const line = buffered.slice(0, newline).replace(/\r$/, "");
          buffered = buffered.slice(newline + 1);
          if (line.length > 0) {
            this.#handleMessage(JSON.parse(line) as JsonObject);
          }
          newline = buffered.indexOf("\n");
        }
        if (done) {
          break;
        }
      }
      if (buffered.trim().length > 0) {
        this.#handleMessage(JSON.parse(buffered) as JsonObject);
      }
    } catch (error) {
      failure = error instanceof Error ? error : new Error(String(error));
    } finally {
      reader.releaseLock();
      const reason = failure ?? new Error("sandbox server stdout closed");
      for (const pending of this.#pending.values()) {
        pending.reject(reason);
      }
      this.#pending.clear();
    }
  }

  #handleMessage(message: JsonObject): void {
    if (typeof message.id === "number") {
      const pending = this.#pending.get(message.id);
      if (pending === undefined) {
        return;
      }
      this.#pending.delete(message.id);
      if ("error" in message) {
        const error = message.error as RpcFailure;
        pending.reject(
          new Error(`sandbox RPC ${error.code}: ${error.message}`),
        );
      } else {
        pending.resolve(message.result);
      }
      return;
    }
    if (typeof message.id === "string" && typeof message.method === "string") {
      void this.#handleServerRequest(
        message.id,
        message.method,
        message.params,
      );
      return;
    }
    if (typeof message.method === "string") {
      this.#onNotification(message.method, message.params);
    }
  }

  async #handleServerRequest(
    id: string,
    method: string,
    params: unknown,
  ): Promise<void> {
    try {
      const result = await this.#onRequest(method, params);
      await this.#write({ id, result });
    } catch (error) {
      await this.#write({
        id,
        error: {
          code: -32603,
          message: error instanceof Error ? error.message : String(error),
        },
      });
    }
  }
}

export async function execWithApproval(
  client: SandboxServerClient,
  params: JsonObject,
  additionalPermissions: AdditionalPermissionProfile,
): Promise<CommandExecOutcome> {
  return await client.request<CommandExecOutcome>("command/exec", {
    ...params,
    additionalPermissions,
  });
}

function readOnlyPermissionProfile(): JsonObject {
  return {
    type: "managed",
    file_system: {
      type: "restricted",
      entries: [
        {
          path: {
            type: "special",
            value: { kind: "root" },
          },
          access: "read",
        },
      ],
    },
    network: "restricted",
  };
}

if (import.meta.main) {
  const binary = Deno.env.get("CODEX_SANDBOX_SERVER") ??
    "./dist/codex-sandbox-server";
  const linuxSandboxExe = Deno.env.get("CODEX_LINUX_SANDBOX") ?? undefined;
  const stateDirectory = `${Deno.cwd()}/.sandbox-server-example-state`;
  const statePath = `${stateDirectory}/state.txt`;
  await Deno.mkdir(stateDirectory, { recursive: true });
  const client = await SandboxServerClient.start({
    binary,
    cwd: Deno.cwd(),
    permissionProfile: readOnlyPermissionProfile(),
    linuxSandboxExe,
    networkProxyConfig: {
      enabled: true,
      mode: "full",
      domains: {
        "api.github.com": "allow",
        "169.254.169.254": "deny",
      },
      allow_local_binding: false,
    },
    onRequest: async (method, params) => {
      if (method === "command/exec/requestPermissionsApproval") {
        console.error(
          "model proposed additional permissions; human review required",
          params,
        );
        // The model/business layer proposes paths. A human only approves or declines.
        const approved = true; // Replace with the parent service's real review UI.
        return { decision: approved ? "accept" : "decline" };
      }
      if (method === "command/exec/requestNetworkApproval") {
        console.error("network access requires human review", params);
        return { decision: "acceptForSession" };
      }
      throw new Error(`unsupported sandbox server request: ${method}`);
    },
    onNotification: (method, params) => {
      console.error("sandbox notification", method, params);
    },
  });

  try {
    const result = await execWithApproval(
      client,
      {
        command: [
          "sh",
          "-c",
          'printf \'hello from sandbox\\n\' > "$1"; cat "$1"',
          "sh",
          statePath,
        ],
        processId: "write-example-state",
      },
      {
        fileSystem: {
          entries: [{
            path: { type: "path", path: stateDirectory },
            access: "write",
          }],
        },
      },
    );
    console.log(result);
  } finally {
    const status = await client.close();
    if (!status.success) {
      throw new Error(`sandbox server exited with code ${status.code}`);
    }
  }
}

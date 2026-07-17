# 构建与发布

本文档说明如何构建可复制到其他 Linux 主机的静态二进制，以及如何通过 GitHub Actions 发布
版本化和 latest Release assets。

## 本地静态构建

正式发布应从仓库根目录构建 musl static PIE 包：

```bash
nix build .#sandbox-server
```

Linux 上的 `sandbox-server` 默认指向当前 CPU 架构对应的静态包，也可以显式构建：

```bash
nix build .#sandbox-server-static
```

构建结果同时包含：

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

`file` 应显示 `static-pie linked` 或 `statically linked`，且 ELF 不应包含动态 interpreter。
`ldd` 通常显示 `statically linked`，但具体文本和退出码可能因架构与发行版而不同。静态包支持
`x86_64-linux` 和 `aarch64-linux`，产物不能跨 CPU 架构运行。

日常开发仍可用 Cargo 构建本机动态版本，但不要发布 `target/release` 里的 Nix dev-shell
产物；它们可能包含写死的 `/nix/store` interpreter/RUNPATH。

## GitHub Release workflow

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
codex-sandbox-server-x86_64-unknown-linux-musl.tar.gz
codex-sandbox-server-x86_64-unknown-linux-musl.tar.gz.sha256
codex-sandbox-server-aarch64-unknown-linux-musl.tar.gz
codex-sandbox-server-aarch64-unknown-linux-musl.tar.gz.sha256
SHA256SUMS
manifest.json
```

同一版本不会被重新发布或覆盖。内容有变化时必须递增版本号，以保证已被其他项目 pin 的 URL 和
SHA-256 始终表示相同字节。

## 下载与校验

Release 会被标记为 GitHub latest。需要自动跟随新版本的项目可以使用固定 asset URL：

```text
https://github.com/hxx0215/codex/releases/latest/download/codex-sandbox-server-x86_64-unknown-linux-musl.tar.gz
https://github.com/hxx0215/codex/releases/latest/download/codex-sandbox-server-aarch64-unknown-linux-musl.tar.gz
```

固定 URL 的内容会随 latest release 改变，适合自动更新。要求构建可复现时，应使用版本化 URL：

```text
https://github.com/hxx0215/codex/releases/download/sandbox-server-v0.1.0/codex-sandbox-server-0.1.0-x86_64-unknown-linux-musl.tar.gz
```

下载 archive 和对应 `.sha256` 后校验：

```bash
sha256sum --check codex-sandbox-server-x86_64-unknown-linux-musl.tar.gz.sha256
```

也可以下载完整 `SHA256SUMS` 或读取 `manifest.json`。manifest 同时提供版本化的 `file`、固定的
`latestFile` 和 SHA-256，便于父服务按 architecture 选择并缓存产物。

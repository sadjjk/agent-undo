# agent-undo (`au`) — 中文版

[English](README.md)

> [peaktwilight/agent-undo](https://github.com/peaktwilight/agent-undo) v0.0.4 的 fork
> 新增：**hook stdin 支持 `agent` 字段**

## 改了什么

`au hook pre` 的 stdin JSON 新增可选的 `agent` 字段：

```json
{
  "session_id": "abc123",
  "tool_name": "Write",
  "agent": "openclaw/feishu",
  "tool_input": { "file_path": "/abs/path.rs" }
}
```

- `agent` **可选** — 不传则 fallback `"claude-code"`，向后兼容
- 第三方集成可传入自己的标识实现正确归因（如 `"openclaw/feishu"`、`"openclaw/webchat"`）

### 改动文件

| 文件 | 改动 |
|------|------|
| `src/hook.rs` | `ClaudeHookInput` 加 `agent: Option<String>`；`run_pre()` 读取并 fallback |
| `ARCHITECTURE.md` | 更新 stdin schema + agent 字段说明 |

## 为什么需要

OpenClaw 通过 `au hook pre/post` 做文件变更归因。
没有这个字段时，所有 OpenClaw session 在 `au log` / `au sessions` 里都显示 `"claude-code"`。
有了它，每个渠道（飞书/webchat/cron 等）都有独立的 agent 名称，归因准确。

## 安装

**方式一：下载预编译二进制**

从 [Releases](../../releases) 下载对应平台的 `au` 二进制，放到 PATH 下即可（如 `~/.local/bin/`）。

**方式二：自行编译**

需要 Rust 工具链（[rustup](https://rustup.rs)）：

```sh
git clone -b feat/hook-agent-field https://github.com/sadjjk/agent-undo.git
cd agent-undo
cargo build --release
cp target/release/au ~/.local/bin/au
```

## 完整文档

见上游 [README.md](README.md)。

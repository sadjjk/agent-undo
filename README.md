<p align="center">
  <img src="assets/github-banner.svg" alt="agent-undo banner" width="100%">
</p>

# agent-undo (`au`) — 中文版

[English](README_en.md)

> [peaktwilight/agent-undo](https://github.com/peaktwilight/agent-undo) v0.0.4 的 fork

## 介绍

agent-undo（`au`）是本地优先的 AI 编码回滚工具，快照每次文件写入，一键撤销任意 session。

本 fork 在上游基础上增加了以下功能：

- **[v0.0.4.2]** 文件删除事件归因修复，不再硬编码 `"unknown"`
- **[v0.0.4.1]** hook stdin 支持自定义 `agent` 字段，不再硬编码 `"claude-code"`

完整文档见上游 [README_en.md](README_en.md)。

## 安装

**方式一：下载预编译二进制**

从 [Releases](../../releases) 下载对应平台的 `au` 二进制，放到 PATH 下即可（如 `~/.local/bin/`）。下载后需加执行权限：`chmod +x ~/.local/bin/au`

**方式二：自行编译**

需要 Rust 工具链（[rustup](https://rustup.rs)）：

```sh
git clone -b feat/hook-agent-field https://github.com/sadjjk/agent-undo.git
cd agent-undo
cargo build --release
cp target/release/au ~/.local/bin/au
```

## 修改记录

### v0.0.4.2

**背景**：上游 `src/daemon.rs` 中 `process_path()` 处理删除事件时，归因硬编码为 `"unknown"`，跳过了 `resolve_attribution()` 调用。即使 `au hook pre` 已写入 marker，删除事件仍无法归因到对应 agent。

**改动文件**：

| 文件 | 改动 |
|------|------|
| `src/daemon.rs` | 删除分支调用 `resolve_attribution(store)` 替代硬编码 `"unknown"`；有 marker 时正确归因，无 marker 时 fallback 行为不变 |

**使用方法**：

无需额外操作。`au hook pre` 写入 marker 后，同一 session 内的文件删除事件自动归因到对应 agent。

### v0.0.4.1

**背景**：上游 `src/hook.rs` 中 `run_pre()` 的 agent 硬编码为 `"claude-code"`，导致所有通过 `au hook pre` 传入的 session 归因都显示为 Claude Code，非 Claude Code 集成方无法区分来源。

**改动文件**：

| 文件 | 改动 |
|------|------|
| `src/hook.rs` | `ClaudeHookInput` 加 `agent: Option<String>`（`#[serde(default)]`）；`run_pre()` 两处 `"claude-code".into()` 改为 `input.agent.clone().unwrap_or_else(\|\| "claude-code".into())`；metadata 中 `"tool"` 同理 |
| `ARCHITECTURE.md` | stdin schema 加 `agent` 字段；描述补充 agent 可选说明 |

**使用方法**：

`au hook pre` 的 stdin JSON 新增可选 `agent` 字段：

```json
{
  "session_id": "abc123",
  "tool_name": "Write",
  "agent": "openclaw/feishu",
  "tool_input": { "file_path": "/abs/path.rs" }
}
```

- `agent` **可选** — 不传则 fallback `"claude-code"`，向后兼容
- 传入后 `au log` / `au sessions` / `au blame` 均按该 agent 归因

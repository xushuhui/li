# M3：会话续接与崩溃恢复

Status: ready-for-agent

## Problem Statement

M1 起每次启动都新建一个会话文件。上一个进程聊过什么，下一个进程完全不知道——用户要重新解释上下文，模型要重新读文件。

design.md 的 G3 要求「关掉后能继续上次对话」，G4 要求「`kill -9` 之后会话不丢」。

会话文件的写入、加载、树回溯（`parentId`）在 M1 已经实现。本里程碑缺的是**入口**：让 CLI 能找到旧会话、打开它、把它当成当前对话继续写。

验收标准：`baize "帮我看下 main"`，退出，再 `baize --continue "刚才说的那个函数"`，第二轮请求里带着第一轮的消息。

## Solution

三层改动：

1. `crates/core` 增加会话发现（列目录 + 读摘要）和加载时的配对修复。
2. `crates/cli` 增加 `--continue` 与 `--resume` 两个启动参数。
3. 锁从「锁会话文件本身」改为「锁 `<文件>.lock`」，让浏览列表时不阻塞。

## User Stories

1. 作为用户，我希望 `baize --continue` 直接接着当前目录上一个会话聊，这样不用每次重新交代背景。
2. 作为用户，我希望 `--continue` 只看当前目录的会话，这样在多个项目之间切换时不会串味。
3. 作为用户，我希望 `baize --resume` 列出最近 10 个会话让我选，这样能回到昨天那个话题。
4. 作为用户，我希望列表里显示每个会话的标题、时间和目录，这样我能认出是哪一个。
5. 作为用户，我希望标题来自我发的第一句话，这样不用额外调用模型生成。
6. 作为用户，我希望会话文件被强杀留下半行 JSON 时仍能打开，这样最需要恢复的时候不会失败。
7. 作为用户，我希望上次没跑完的工具调用在恢复时被补成「执行被中断」，这样模型看到的工具调用和结果始终成对。
8. 作为用户，我希望两个终端同时打开同一个会话时第二个明确报错，这样不会有两份内容交错写进同一个文件。
9. 作为用户，我希望当前目录没有任何会话时 `--continue` 给出明确提示，而不是静默新建一个空会话。
10. 作为用户，我希望继续的会话把旧消息一起带给模型，这样它记得之前读过哪些文件。
11. 作为用户，我希望续接之后新消息追加在原文件末尾，而不是另起一个文件。
12. 作为用户，我希望恢复会话时不改变文件的树结构，这样后续 `/tree` 之类的能力仍有完整数据。

## Implementation Decisions

### CLI 参数

```rust
/// 继续当前目录最近的会话
#[arg(long = "continue", conflicts_with = "resume")]
continue_session: bool,

/// 从最近 10 个会话里选一个继续
#[arg(long, conflicts_with = "continue")]
resume: bool,
```

两者互斥（`clap` 的 `conflicts_with`）。都不给就是新建会话，行为与现在一致。

`--continue` 与一次性 prompt 可以组合：`baize --continue "接着讲"`。

### 会话发现

`crates/core/src/session.rs` 新增：

```rust
pub struct SessionSummary {
    pub path: PathBuf,
    pub id: String,
    pub cwd: String,
    pub timestamp: String,
    pub title: String,
}

pub fn list_summaries(dir: &Path, limit: usize) -> Result<Vec<SessionSummary>>;
pub fn latest_summary(dir: &Path) -> Result<Option<SessionSummary>>;
```

实现：读目录，只取 `.jsonl` 后缀，按文件名倒序（文件名以 `2026-11-06T13-55-42Z_` 开头，字典序即时间序），取前 `limit` 个。

每个文件只读到「会话头 + 第一条用户消息」为止，不读全文。文件打不开或两样都凑不齐就跳过该文件，不中断整体列表。

标题：第一条用户消息的文本，换行替换成空格，按 `char` 截断到 40 个，超出加 `…`。

时间：会话头的 `timestamp` 是 RFC3339（`2026-11-06T13:55:42Z`），列表里取前 16 个字符并把 `T` 换成空格，显示到分钟。

这一步**不拿锁**，纯读。所以浏览列表不会导致别的终端无法打开。

### `--continue`

取当前工作目录对应的会话目录，找最新一个。

- 有 → `Session::load(path)`。
- 没有 → 标准错误输出「当前目录还没有会话，直接运行 `baize` 开一个新的」，退出码 1。

### `--resume`

没有会话时同 `--continue` 的提示。

有会话时打印：

```
 1  2026-11-06 13:55  /Users/xsh/gp/dotfiles  帮我修 ghs 函数的排序
 2  2026-11-05 09:12  /Users/xsh/gp/dotfiles  看看 nvim 配置
选择 [1-2，直接回车取消]:
```

读一行标准输入。空行或无法解析 → 退出码 0，不打开任何会话。编号越界 → 重新提示一次，再错就退出。

选中后 `Session::load(path)`。

### 打开与锁

`Session::create` 与 `Session::load` 都改为锁 `<文件>.lock` 而不是文件本身：

- `create`：创建 `<stamp>_<id>.jsonl`，同时创建同名 `.lock` 并加排他锁。
- `load`：打开 `<文件>.lock` 加排他锁，再打开会话文件读内容。

获取锁失败时错误信息包含会话路径，并提示「另一个 baize 进程正在使用这个会话」。

会话文件本身以 `append(true).read(true)` 打开，不再持有锁。

### 配对修复

`Session::load` 在读完条目后调用 `repair_dangling_tool_calls()`：

1. 沿当前可见路径收集所有 `toolCall` 的 `(id, name)`。
2. 收集所有 `toolResult` 的 `toolCallId`。
3. 差集里的每个调用追加一条 `toolResult`：内容「执行被中断」，`isError: true`。

返回值是补了几条，CLI 在标准错误打印「已补全 N 个未完成的工具调用」。

修复发生在 `load` 内部而不是调用方，因为「打开一个会话」的语义就应该是「得到一个可以继续用的会话」。

### 续接后的行为

续接的会话文件继续追加，不新建。系统提示按当前时刻重新生成（时间、规则文件内容都取当前值），历史消息作为上下文一起发给模型。

模型选择：仍然用配置里的默认模型或 `--model`，不读会话里的 `model_change` 记录。`model_change` 只作为历史记录保留。

## Testing Decisions

### core 单元测试（`crates/core/src/session.rs`）

1. 加载时跳过损坏行：文件中插入一行 `{broken`，其余条目正常读出。
2. 配对修复：预置带 `toolCall` 但没有结果的会话，`load` 后补出 `isError: true` 的结果，且再 `load` 一次不会重复补。
3. `list_summaries`：写三个会话文件，返回顺序是新的在前，标题取首条用户消息。
4. 标题截断：41 个字符的首条消息被截到 40 个加省略号。
5. 锁冲突：同一进程两次 `load` 同一文件，第二次报错。
6. 列表读取不拿锁：`list_summaries` 之后 `Session::load` 仍能成功。

### 命令行集成测试（`crates/cli/tests/m3_session.rs`）

沿用假模型服务器，但 `HOME` 指向临时目录：

7. `--continue` 无会话：退出码 1，标准错误含「还没有会话」。
8. `--continue` 带历史：先跑一次普通对话，再 `--continue`，第二个请求的 `messages` 里包含第一轮的 user 与 assistant 文本。
9. `--continue` 追加写：两次运行后只有一个 `.jsonl` 文件，且条目数增加。
10. `--resume` 列表与选择：预置两个会话，输入 `1`，选中较新的那个。
11. `--resume` 直接回车：退出码 0，不产生请求。
12. 崩溃恢复：预置一个带未配对 `toolCall` 的会话文件，`--continue` 后第二个请求里存在 `tool_call_id` 对应、内容含「执行被中断」的 tool 消息。

## Out of Scope

- 会话索引（`index.json`）——design.md 的 D2 排在会话数超过约 2000 个之后。
- 跨进程恢复 `read` 的内容快照（恢复后要重新 `read` 才能 `edit`）。
- 会话删除、重命名、导出。
- `/tree`、`/branch` 等树操作界面。
- 多模型角色恢复。

## Further Notes

**为什么要改锁文件名**：锁会话文件本身时，任何读取（包括列表预览）都会与写进程冲突。锁独立的 `.lock` 文件后，读永远可以，只有「打开并继续」才互斥。会话目录里会累积空的 `.lock` 文件，不影响列表（只扫 `.jsonl`）。

**为什么标题不调用模型**：design.md 的 D22 已经否决了自动标题——多一次请求，且中文场景下自动标题往往比原文更模糊。

**续接时系统提示重新生成**：会话里不存系统提示，因为规则文件（`AGENTS.md`）可能在两次对话之间被改过。每次启动都按当前内容重建，这是有意的。

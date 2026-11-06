# search-tools：`grep` 与 `glob`

Status: ready-for-agent

## Problem Statement

M2 之后，baize 能读写文件、能跑命令，但**找东西只能靠 `bash` 拼 `rg`**。这带来三个问题：

1. 模型必须先知道文件在哪，才能 `read`。仓库大了它只能靠猜。
2. `bash rg` 的输出是给人看的文本，模型要自己数行、自己切分，容易漏。
3. `bash` 对非白名单命令要审批，搜索这种纯读操作不该打断用户。

补上两个只读工具，模型就能先搜后读，而不是盲读。

验收标准：让它在一万行以上的仓库里找某个函数的定义，它先 `grep` 定位再 `read` 那一小段，不需要扫描整个目录。

## 里程碑归属

design.md 第 8 节的 M3 是「会话持久化」，本 feature 不占里程碑编号，是 M2 工具集的补全。design.md 第 7 节的工具表已经列出 `grep` 和 `glob`，本 spec 落实它们。

## Solution

两个工具共用一个遍历层：`ignore` crate 的 `WalkBuilder`——ripgrep 自己用的目录遍历器，默认遵守 `.gitignore`、`.ignore`、`.git/info/exclude` 和全局 gitignore。

- `grep` 在这个遍历上跑 `grep-searcher` 的逐行正则搜索，返回 `路径:行号:内容`。
- `glob` 在这个遍历上用 `globset` 匹配文件名，返回按修改时间倒序的路径。

不用 `regex` crate 手写遍历：它会走进 `target/`、`node_modules/`，在 dotfiles 仓库里文件数从 145 涨到 4826。

## User Stories

1. 作为用户，我希望模型能按正则搜索仓库内容，这样我不用告诉它文件在哪。
2. 作为用户，我希望搜索结果带文件名和行号，这样模型能精确 `read` 那一小段。
3. 作为用户，我希望搜索默认跳过 `.gitignore` 里的路径，这样构建产物不会淹没结果。
4. 作为用户，我希望搜索结果最多 100 条并提示「还有更多」，这样上下文不会被一次搜索撑爆。
5. 作为用户，我希望可以按文件名过滤搜索范围（只搜 `*.rs`），这样减少无关命中。
6. 作为用户，我希望搜索跳过二进制文件，这样不会把编译产物的乱码塞进上下文。
7. 作为用户，我希望正则写错时得到明确的语法错误，而不是空结果。
8. 作为用户，我希望模型能按 glob 模式列出文件（`**/*.testing.rs`），这样它能发现项目结构。
9. 作为用户，我希望 glob 结果按修改时间倒序，这样最近改过的文件排在前面。
10. 作为用户，我希望 glob 最多返回 200 条，这样大仓库不会刷屏。
11. 作为用户，我希望两个搜索工具都算只读操作，同一轮里可以并行执行且不需要审批。
12. 作为用户，我希望搜索没有结果时是正常返回而不是报错，这样模型会换关键词重试。

## Implementation Decisions

### 依赖

`crates/tools` 新增三个：

| crate | 版本 | 用途 |
|---|---|---|
| `grep` | 0.4 | `grep-searcher` + `grep-regex` 的再导出 |
| `ignore` | 0.4 | 遵守 gitignore 的并行目录遍历 |
| `globset` | 0.4 | glob 模式编译与匹配（`ignore` 已传递依赖，显式声明因为要用它的 API） |

design.md 第 9 节依赖表里 `regex` 一行删除——`grep` 内部已经带 `grep-regex`，不直接依赖 `regex`。

### 共同：遍历

两个工具都从 `WalkBuilder::new(root)` 开始，使用默认配置：

- 尊重所有 ignore 文件
- 跳过隐藏文件和目录
- 不跟随符号链接
- `require_git(false)`——不是 git 仓库的目录也要能搜

### grep

输入：

| 参数 | 必需 | 说明 |
|---|---|---|
| `pattern` | 是 | Rust 正则语法 |
| `path` | 否 | 根目录或单个文件，默认工作目录 |
| `include` | 否 | 文件名 glob，如 `*.rs`；不含 `/` 时匹配任意深度 |
| `ignore_case` | 否 | 默认大小写敏感 |
| `limit` | 否 | 默认 100，上限 1000 |

执行：`WalkBuilder::build_parallel()` 并行遍历，每个文件用 `SearcherBuilder` 配置 `line_number(true)` 和 `BinaryDetection::quit(b'\x00')`，配一个自定义 `Sink` 收集命中。

命中上限用 `AtomicUsize` 计数，达到上限后 `Sink::matched` 返回 `false` 停止该文件，遍历闭包返回 `WalkState::Quit` 停止整体。`limit` 只影响展示，不影响计数逻辑。

输出：按 `路径:行号:内容` 每行一条，路径相对搜索根目录。行内容去掉行尾换行符；非 UTF-8 字节用替换字符处理。结果按路径、行号排序后再输出（并行遍历顺序不确定，排序让输出可复现）。

达到上限时追加一行：`[已显示 100 条，可能还有更多，请收窄 pattern 或 path]`。

没有命中时返回 `没有匹配`，`is_error: false`。

### glob

输入：

| 参数 | 必需 | 说明 |
|---|---|---|
| `pattern` | 是 | globset 语法，支持 `**` |
| `path` | 否 | 根目录，默认工作目录 |
| `limit` | 否 | 默认 200，上限 2000 |

语义：`Glob::new(pattern)` 用默认配置，`literal_separator` 为 `false`，因此 `*.rs` 会匹配 `src/deep/a.rs`（与 ripgrep 的 `-g` 一致）。模式不自动加前缀，用户写 `*.rs` 就是「任意深度的 .rs 文件」。

执行：并行遍历，对每个**普通文件**调用 `GlobSet::is_match`。命中后 `entry.metadata()` 取修改时间。

输出：按修改时间倒序，每行一条相对路径。超过上限截断并追加 `[已显示 200 条，共 N 条匹配]`。

目录不进入结果。

排序需要 stat 每个命中文件。普通仓库里匹配数在几百量级，成本可忽略；匹配数上万时会多花约一秒——见 SIMPLIFICATION 注释。

### 只读并行

`crates/cli/src/main.rs` 的 `is_read_only` 增加 `grep`、`glob` 两个名字，它们进入并行执行组。

### 阻塞边界

`Searcher` 和 `WalkBuilder` 都是同步阻塞 I/O。工具的执行方法内部用 `tokio::task::spawn_blocking` 包住整个遍历，避免占住 tokio 的 worker 线程。这是 M2 的 `read`/`bash` 没有的问题，因为异步文件读写和子进程本身就是非阻塞的。

### 错误

- 正则/glob 语法错误：`is_error: true`，内容包含具体错误位置。
- 路径不存在：`is_error: true`，内容为「无法访问 <路径>」。
- 没有匹配：`is_error: false`。

## Testing Decisions

沿用 M2 的接缝：工具层用临时目录直接调用，命令行层用真实二进制 + 假模型服务器。

工具层（`crates/tools`，用现有 `test_support::Rig`）：

1. `grep` 命中：写两个文件，只在一个里有匹配，输出包含正确路径和行号。
2. `grep` 跳过 gitignore：写 `.gitignore` 忽略 `generated/`，里面的匹配不出现。
3. `grep` 上限：写 150 行匹配，默认上限 100，输出 100 条带提示。
4. `grep` `include` 过滤：只返回 `.rs` 文件里的命中。
5. `grep` 二进制跳过：含 NUL 的文件不产生命中且不报错。
6. `grep` 正则错误：`(` 返回 `is_error`。
7. `grep` 大小写：`ignore_case` 开关行为正确。
8. `glob` 跨目录：`*.txt` 匹配 `a.txt` 和 `sub/b.txt`。
9. `glob` 递归：`**/*.rs` 匹配深层文件。
10. `glob` 不返回目录：目录名匹配模式时不出现在结果里。
11. `glob` 上限：写 250 个文件，默认上限 200。
12. `glob` 修改时间排序：先写旧文件、后写新文件，新的排在前面。

命令行层（`crates/cli/tests/m3.rs`，复用 `m2.rs` 的假服务器工具函数）：

13. 模型调用 `grep`，第二轮请求里带上命中内容。
14. 模型调用 `glob`，第二轮请求里带上文件名。

## Out of Scope

- 上下文行（`-C`）、替换、按文件计数模式。
- 符号链接跟随、超大文件跳过阈值。
- 搜索结果缓存（跨进程文件索引）。
- 按 mtime 流式排序（先收集再排序，见 SIMPLIFICATION）。
- `.gitignore` 之外的自定义 ignore 文件配置。

## Further Notes

**SIMPLIFICATION（glob 排序）**：先收集全部匹配的 `(mtime, path)` 再排序截断。匹配数超过一万时 stat 开销约一秒。升级路径是维护一个 `limit` 大小的最小堆，遍历中随时淘汰较旧项——但这不减少 stat 次数，只减少内存，所以收益有限，暂不做。

**SIMPLIFICATION（grep 提前退出）**：`limit` 达到后停止遍历。这意味着命中数被限制在恰好 `limit` 条，而不是「先全搜完再截断」。对模型来说够用，且避免了在大仓库上白跑。

**为什么不用 `rg` 子进程**：`rg --json` 是逐行流式输出，用 `Command::output()` 一次性读会 OOM；路径在非 UTF-8 场景下变成 `bytes` 数组；退出码 `0/1/2` 三种语义；超时要自己 kill。四项隐藏成本超过 14 秒编译时间和 2.7 MB 二进制增量。

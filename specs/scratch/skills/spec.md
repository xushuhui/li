# skills：技能元数据进系统提示

Status: implemented

## Problem Statement

`~/.agents/skills` 下有 22 个带 `name` / `description` 的 `SKILL.md`，li 启动时完全不知道它们存在。用户在 li 里说"按 go-rules 审一下"，模型只能靠猜，或者根本不知道有这份规则。

同时系统提示里没有模型标识，模型被问到时按训练数据里的身份回答。

## Solution

启动扫描一次，把每个技能的 `name`、`description`、`SKILL.md` 绝对路径拼进系统提示；正文不进提示，模型判断相关时用现有 `read` 打开。这不是新概念——design.md 的 D30 已经定了这个方案，本 spec 只负责落地。

模型名加进提示的固定开场白，和 D32 一起做。

## Implementation Decisions

- **扫描范围**：从当前目录向上到仓库根（遇到含 `.git` 的目录停）每一级的 `.agents/skills/*/SKILL.md`，加用户级 `$HOME/.agents/skills/*/SKILL.md`。同名技能离当前目录越近优先级越高。
- **只读 frontmatter**：`name` 缺失时退回目录名，`description` 缺失时留空。没有 frontmatter 的文件仍然按目录名收录——静默丢掉用户写的技能比多列一条更糟。
- **排序**：按名字排序，保证同一台机器上两次启动的提示一致。
- **截断**：单条描述 100 字符，最多 64 条，超出部分写一行"另有 N 个未列出"。
- **位置**：技能块放在项目规则之后。越靠末尾的常驻内容在长对话里越不容易被忽略。
- **预留量**：`SYSTEM_AND_TOOLS_TOKENS` 从 800 提到 1200（D30 的要求），压缩估算跟着用新值。
- **内置开场白外置**：四行固定文本移到 `crates/core/src/system_prompt.md`，`include_str!` 编译进二进制，运行时替换 `{{cwd}}`、`{{now}}`、`{{model}}`。

## Slash commands

按 D29 新增 `crates/cli/src/slash.rs`，在输入到达模型之前分流：

- `/help`：打印命令列表和已发现的技能。
- `/exit`：退出（等价于交互里直接输入 `exit`）。
- `/skill:<名字> [参数]`：读 `SKILL.md`、剥掉 frontmatter，把正文、技能目录绝对路径和参数作为一条用户消息注入。
- 其他以 `/` 开头的输入（例如 `/usr/local/bin`）原样发给模型。

提示：输入框内容以 `/` 开头时，状态栏列出模糊匹配的命令——前缀匹配排在前，其余按字符顺序（子序列）匹配；第一个为选中项，按 Tab 填入输入框。最多显示 6 条，超出时显示总数。REPL 与 TUI 共用同一个分流函数。

## Testing Decisions

core 层（`crates/core/src/skills.rs`）：

1. 读取 frontmatter 的 name 与 description，去掉引号。
2. 没有 frontmatter 时退回目录名。
3. 项目级同名技能覆盖用户级。
4. 从嵌套目录向上找到仓库根的技能。
5. 没有 `SKILL.md` 的目录被跳过。

core 层（`crates/core/src/prompt.rs`）：

6. 提示里出现"当前模型："这一行。
7. 技能块排在项目规则之后，包含名字、路径和描述。
8. 超长描述被截断到 100 字符并加省略号。
9. 超过 64 条时只列 64 条并提示剩余数量。

命令行层（`crates/cli/tests/m1.rs`）：

10. 项目内的技能出现在发给模型的系统提示里。
11. `--model` 指定的模型名出现在系统提示里。
12. `/help` 直接打印命令列表，不向模型发请求。

斜杠命令层（`crates/cli/src/slash.rs`）：

13. 普通文本原样发给模型。
14. 未注册的 `/路径` 原样发给模型。
15. `/help` 列出命令和技能。
16. `/skill:` 注入正文、去掉 frontmatter、带上目录和参数。
17. 技能名不存在时提示去看 `/help`。
18. `/exit` 返回退出动作。
19. 候选列表：前缀匹配排在前面，子序列能命中不连续的字符，非斜杠输入返回空。

TUI（`crates/cli/src/tui.rs`）：

20. 输入以 `/` 开头时状态栏出现候选行。
21. 按 Tab 把第一个候选填进输入框。

## Out of Scope

- `/init`（属于 D31，需要内置提示词与只读工具约束）。
- 技能注册表、远程分发。
- 会话中途重新扫描技能目录。

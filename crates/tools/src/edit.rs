//! `edit`: line-anchored patches validated against a recorded snapshot.

use serde_json::{Value, json};

use crate::snapshot::tag_of;
use crate::{Tool, ToolContext, ToolOutcome};

pub struct EditTool;

#[derive(Clone)]
struct Section {
    path: String,
    tag: String,
    ops: Vec<Op>,
}

#[derive(Clone)]
enum Op {
    Replace {
        start: usize,
        end: usize,
        body: Vec<String>,
    },
    InsertBefore {
        at: usize,
        body: Vec<String>,
    },
    InsertAfter {
        at: usize,
        body: Vec<String>,
    },
    Append {
        body: Vec<String>,
    },
    Cut {
        start: usize,
        end: usize,
    },
}

impl Op {
    fn range(&self) -> (usize, usize) {
        match self {
            Op::Replace { start, end, .. } | Op::Cut { start, end } => (*start, *end),
            Op::InsertBefore { at, .. } | Op::InsertAfter { at, .. } => (*at, *at),
            Op::Append { .. } => (usize::MAX, usize::MAX),
        }
    }
}

#[async_trait::async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "按行锚点修改文件。输入形如 [路径#标签] 后跟 PUT/CUT 操作。标签来自最近一次 read。"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "一个或多个 [路径#标签] 段。示例：\n[src/main.rs#1A2B]\nPUT 4.=4:\n+let value = 2;"
                }
            },
            "required": ["input"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolOutcome {
        let Some(input) = args["input"].as_str() else {
            return ToolOutcome::error("缺少 input 参数");
        };

        let sections = match parse(input) {
            Ok(sections) => sections,
            Err(error) => return ToolOutcome::error(error),
        };

        let mut results = Vec::new();
        for section in &sections {
            match apply_section(section, ctx) {
                Ok(line) => results.push(line),
                Err(error) => return ToolOutcome::error(format!("{}：{error}", section.path)),
            }
        }
        ToolOutcome::ok(results.join("\n"))
    }
}

fn apply_section(section: &Section, ctx: &ToolContext) -> Result<String, String> {
    let full = ctx.resolve(&section.path);
    let original =
        std::fs::read_to_string(&full).map_err(|error| format!("无法读取文件：{error}"))?;

    {
        let store = ctx
            .snapshots
            .lock()
            .map_err(|_| "快照锁失败".to_string())?;
        let snapshot = store
            .find(&full, &section.tag)
            .ok_or_else(|| format!("未知标签 {}，请先 read 该文件", section.tag))?;

        if tag_of(&original) != snapshot.tag {
            return Err("文件内容已变化，请重新 read 后再编辑".to_string());
        }

        for op in &section.ops {
            if matches!(op, Op::Append { .. }) {
                continue;
            }
            let range = op.range();
            if !store.contains_visible(snapshot, range) {
                return Err(format!("第 {} 行不在已读取的范围内", range.0));
            }
        }
    }

    check_overlaps(&section.ops)?;

    let trailing_newline = original.ends_with('\n');
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    apply_ops(&mut lines, section.ops.clone())?;

    let mut updated = lines.join("\n");
    if trailing_newline && !lines.is_empty() {
        updated.push('\n');
    }

    std::fs::write(&full, &updated).map_err(|error| format!("无法写入：{error}"))?;

    let tag = tag_of(&updated);
    let total = lines.len();
    ctx.snapshots
        .lock()
        .map_err(|_| "快照锁失败".to_string())?
        .record(&full, tag.clone(), updated, vec![(1, total.max(1))]);

    Ok(format!("[{}#{tag}]", ctx.display(&full)))
}

fn check_overlaps(ops: &[Op]) -> Result<(), String> {
    let mut ranges: Vec<(usize, usize)> = ops
        .iter()
        .filter(|op| !matches!(op, Op::Append { .. }))
        .map(Op::range)
        .collect();
    ranges.sort();
    for pair in ranges.windows(2) {
        if pair[0].1 >= pair[1].0 {
            return Err(format!(
                "操作范围重叠：{}..{} 与 {}..{}",
                pair[0].0, pair[0].1, pair[1].0, pair[1].1
            ));
        }
    }
    Ok(())
}

fn apply_ops(lines: &mut Vec<String>, ops: Vec<Op>) -> Result<(), String> {
    let (appends, mut others): (Vec<Op>, Vec<Op>) = ops
        .into_iter()
        .partition(|op| matches!(op, Op::Append { .. }));
    others.sort_by_key(|op| std::cmp::Reverse(op.range().0));

    for op in others {
        match op {
            Op::Replace { start, end, body } => {
                if end > lines.len() {
                    return Err(format!("行号 {end} 超出文件范围（共 {} 行）", lines.len()));
                }
                lines.splice(start - 1..end, body);
            }
            Op::InsertBefore { at, body } => {
                if at > lines.len() + 1 {
                    return Err(format!("行号 {at} 超出文件范围（共 {} 行）", lines.len()));
                }
                let index = (at - 1).min(lines.len());
                lines.splice(index..index, body);
            }
            Op::InsertAfter { at, body } => {
                if at > lines.len() {
                    return Err(format!("行号 {at} 超出文件范围（共 {} 行）", lines.len()));
                }
                lines.splice(at..at, body);
            }
            Op::Cut { start, end } => {
                if end > lines.len() {
                    return Err(format!("行号 {end} 超出文件范围（共 {} 行）", lines.len()));
                }
                lines.drain(start - 1..end);
            }
            Op::Append { .. } => unreachable!("append handled separately"),
        }
    }

    for op in appends {
        if let Op::Append { body } = op {
            lines.extend(body);
        }
    }
    Ok(())
}

fn parse(input: &str) -> Result<Vec<Section>, String> {
    let mut sections = Vec::new();
    let mut current: Option<Section> = None;
    let mut pending: Option<(BodyKind, Vec<String>)> = None;

    for (index, raw) in input.lines().enumerate() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }

        if line.starts_with('[') {
            flush(&mut current, &mut pending)?;
            let (path, tag) = parse_header(line)?;
            if let Some(section) = current.take() {
                sections.push(section);
            }
            current = Some(Section {
                path,
                tag,
                ops: Vec::new(),
            });
            continue;
        }

        if let Some(rest) = line.strip_prefix('+') {
            let Some((_, body)) = pending.as_mut() else {
                return Err(format!("第 {} 行：+ 正文没有对应的操作", index + 1));
            };
            body.push(rest.to_string());
            continue;
        }

        flush(&mut current, &mut pending)?;

        let Some(section) = current.as_mut() else {
            return Err(format!("第 {} 行：操作出现在 [路径#标签] 之前", index + 1));
        };

        if let Some(rest) = line.strip_prefix("PUT ") {
            pending = Some(parse_put(rest)?);
        } else if let Some(rest) = line.strip_prefix("CUT ") {
            let (start, end) = parse_range(rest)?;
            section.ops.push(Op::Cut { start, end });
        } else {
            return Err(format!("第 {} 行：无法识别“{line}”", index + 1));
        }
    }

    flush(&mut current, &mut pending)?;
    if let Some(section) = current {
        sections.push(section);
    }
    if sections.is_empty() {
        return Err("没有找到 [路径#标签] 段".to_string());
    }
    Ok(sections)
}

enum BodyKind {
    Replace { start: usize, end: usize },
    InsertBefore(usize),
    InsertAfter(usize),
    Append,
}

fn flush(
    current: &mut Option<Section>,
    pending: &mut Option<(BodyKind, Vec<String>)>,
) -> Result<(), String> {
    let Some((kind, body)) = pending.take() else {
        return Ok(());
    };
    let Some(section) = current.as_mut() else {
        return Err("操作出现在段头之前".to_string());
    };
    if body.is_empty() {
        return Err("PUT 操作缺少正文（至少一行以 + 开头）".to_string());
    }
    match kind {
        BodyKind::Replace { start, end } => section.ops.push(Op::Replace { start, end, body }),
        BodyKind::InsertBefore(at) => section.ops.push(Op::InsertBefore { at, body }),
        BodyKind::InsertAfter(at) => section.ops.push(Op::InsertAfter { at, body }),
        BodyKind::Append => section.ops.push(Op::Append { body }),
    }
    Ok(())
}

fn parse_header(line: &str) -> Result<(String, String), String> {
    let inner = line
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| format!("段头格式应为 [路径#标签]：{line}"))?;
    let (path, tag) = inner
        .rsplit_once('#')
        .ok_or_else(|| format!("段头缺少 #标签：{line}"))?;
    if tag.len() != 4 || !tag.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("标签应为 4 位十六进制：{tag}"));
    }
    if path.is_empty() {
        return Err("段头缺少路径".to_string());
    }
    Ok((path.to_string(), tag.to_uppercase()))
}

fn parse_put(spec: &str) -> Result<(BodyKind, Vec<String>), String> {
    let spec = spec
        .strip_suffix(':')
        .ok_or_else(|| format!("PUT 操作应以冒号结尾：PUT {spec}"))?;

    if let Some(value) = spec.strip_prefix('>') {
        if value == "$" {
            return Ok((BodyKind::Append, Vec::new()));
        }
        return Ok((BodyKind::InsertAfter(parse_line(value)?), Vec::new()));
    }
    if let Some(value) = spec.strip_prefix('<') {
        return Ok((BodyKind::InsertBefore(parse_line(value)?), Vec::new()));
    }
    let (start, end) = parse_range(spec)?;
    Ok((BodyKind::Replace { start, end }, Vec::new()))
}

fn parse_line(value: &str) -> Result<usize, String> {
    let line = value
        .parse::<usize>()
        .map_err(|_| format!("无效行号：{value}"))?;
    if line == 0 {
        return Err("行号从 1 开始".to_string());
    }
    Ok(line)
}

fn parse_range(spec: &str) -> Result<(usize, usize), String> {
    let (left, right) = spec
        .split_once(".=")
        .ok_or_else(|| format!("范围应形如 4.=7：{spec}"))?;
    let start = parse_line(left)?;
    let end = parse_line(right)?;
    if end < start {
        return Err(format!("范围起点大于终点：{spec}"));
    }
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::ReadTool;
    use crate::test_support::{Rig, tag_from};

    #[test]
    fn parses_replace_and_body() {
        let sections = parse("[a.rs#1A2B]\nPUT 2.=2:\n+new\n").unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].path, "a.rs");
        assert_eq!(sections[0].tag, "1A2B");
        assert_eq!(sections[0].ops.len(), 1);
    }

    #[test]
    fn rejects_missing_body() {
        assert!(parse("[a.rs#1A2B]\nPUT 2.=2:\n").is_err());
    }

    #[test]
    fn rejects_body_without_op() {
        assert!(parse("[a.rs#1A2B]\n+orphan\n").is_err());
    }

    #[test]
    fn parses_all_insert_forms() {
        let sections =
            parse("[a.rs#1A2B]\nPUT <1:\n+a\nPUT >1:\n+b\nPUT >$:\n+c\nCUT 2.=3\n").unwrap();
        assert_eq!(sections[0].ops.len(), 4);
    }

    #[test]
    fn applies_replace_in_descending_order() {
        let mut lines = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        apply_ops(
            &mut lines,
            vec![
                Op::Replace {
                    start: 1,
                    end: 1,
                    body: vec!["A".into()],
                },
                Op::Replace {
                    start: 3,
                    end: 3,
                    body: vec!["C".into()],
                },
            ],
        )
        .unwrap();
        assert_eq!(lines, vec!["A", "b", "C"]);
    }

    #[test]
    fn appends_at_end() {
        let mut lines = vec!["a".to_string()];
        apply_ops(
            &mut lines,
            vec![Op::Append {
                body: vec!["z".into()],
            }],
        )
        .unwrap();
        assert_eq!(lines, vec!["a", "z"]);
    }

    #[test]
    fn detects_overlap() {
        let ops = vec![
            Op::Replace {
                start: 1,
                end: 3,
                body: vec!["x".into()],
            },
            Op::Cut { start: 2, end: 4 },
        ];
        assert!(check_overlaps(&ops).is_err());
    }

    async fn read_tag(ctx: &crate::ToolContext, name: &str) -> String {
        let outcome = ReadTool.execute(json!({ "path": name }), ctx).await;
        assert!(!outcome.is_error, "{}", outcome.text);
        tag_from(&outcome.text)
    }

    #[tokio::test]
    async fn replaces_inserts_and_cuts_after_read() {
        let rig = Rig::new();
        rig.write("a.txt", "one\ntwo\nthree\n");
        let ctx = rig.context();
        let tag = read_tag(&ctx, "a.txt").await;

        let patch = format!(
            "[a.txt#{tag}]\nPUT <1:\n+zero\nPUT 2.=2:\n+TWO\nPUT >$:\n+four\nCUT 3.=3\n"
        );
        let outcome = EditTool.execute(json!({ "input": patch }), &ctx).await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(rig.read("a.txt"), "zero\none\nTWO\nfour\n");
        assert!(outcome.text.starts_with("[a.txt#"));
    }

    #[tokio::test]
    async fn rejects_stale_tag() {
        let rig = Rig::new();
        rig.write("a.txt", "one\n");
        let ctx = rig.context();
        let tag = read_tag(&ctx, "a.txt").await;

        rig.write("a.txt", "changed\n");
        let patch = format!("[a.txt#{tag}]\nPUT 1.=1:\n+new\n");
        let outcome = EditTool.execute(json!({ "input": patch }), &ctx).await;

        assert!(outcome.is_error);
        assert!(outcome.text.contains("请重新 read"));
        assert_eq!(rig.read("a.txt"), "changed\n");
    }

    #[tokio::test]
    async fn rejects_unknown_tag_and_unread_lines() {
        let rig = Rig::new();
        rig.write("a.txt", "one\ntwo\nthree\n");
        let ctx = rig.context();

        let unknown = EditTool
            .execute(json!({ "input": "[a.txt#FFFF]\nPUT 1.=1:\n+x\n" }), &ctx)
            .await;
        assert!(unknown.is_error);
        assert!(unknown.text.contains("未知标签"));

        let outcome = ReadTool
            .execute(json!({ "path": "a.txt", "limit": 1 }), &ctx)
            .await;
        let tag = tag_from(&outcome.text);
        let outside = EditTool
            .execute(
                json!({ "input": format!("[a.txt#{tag}]\nPUT 3.=3:\n+x\n") }),
                &ctx,
            )
            .await;
        assert!(outside.is_error);
        assert!(outside.text.contains("已读取"));
        assert_eq!(rig.read("a.txt"), "one\ntwo\nthree\n");
    }

    #[tokio::test]
    async fn edits_two_files_in_one_call() {
        let rig = Rig::new();
        rig.write("a.txt", "a1\n");
        rig.write("b.txt", "b1\n");
        let ctx = rig.context();
        let tag_a = read_tag(&ctx, "a.txt").await;
        let tag_b = read_tag(&ctx, "b.txt").await;

        let patch = format!(
            "[a.txt#{tag_a}]\nPUT 1.=1:\n+A\n[b.txt#{tag_b}]\nPUT 1.=1:\n+B\n"
        );
        let outcome = EditTool.execute(json!({ "input": patch }), &ctx).await;

        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(rig.read("a.txt"), "A\n");
        assert_eq!(rig.read("b.txt"), "B\n");
    }
}

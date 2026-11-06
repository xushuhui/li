//! System prompt assembly: built-in text, then global rules, then project rules.

pub fn build_system_prompt(
    cwd: &str,
    now: &str,
    global_rules: Option<&str>,
    project_rules: &[(String, String)],
) -> String {
    let mut prompt = String::new();
    prompt.push_str("你是 baize，一个在终端里工作的编码助手。\n");
    prompt.push_str(&format!("当前工作目录：{cwd}\n"));
    prompt.push_str(&format!("当前时间：{now}\n"));

    if let Some(rules) = global_rules {
        let rules = rules.trim();
        if !rules.is_empty() {
            prompt.push_str("\n全局规则：\n");
            prompt.push_str(rules);
            prompt.push('\n');
        }
    }

    for (path, content) in project_rules {
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        prompt.push_str(&format!("\n项目规则（{path}）：\n"));
        prompt.push_str(content);
        prompt.push('\n');
    }

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_builtin_then_global_then_project() {
        let project = vec![("/repo/AGENTS.md".to_string(), "PROJECT".to_string())];
        let prompt = build_system_prompt("/tmp", "now", Some("GLOBAL"), &project);
        let builtin = prompt.find("你是 baize").unwrap();
        let global = prompt.find("GLOBAL").unwrap();
        let found = prompt.find("PROJECT").unwrap();
        assert!(builtin < global);
        assert!(global < found);
    }

    #[test]
    fn skips_missing_rules() {
        let prompt = build_system_prompt("/tmp", "now", None, &[]);
        assert!(!prompt.contains("GLOBAL"));
        assert!(prompt.ends_with('\n'));
    }

    #[test]
    fn keeps_every_layer_in_outer_to_inner_order() {
        let project = vec![
            ("/repo/AGENTS.md".to_string(), "ROOT".to_string()),
            ("/repo/pkg/AGENTS.md".to_string(), "PKG".to_string()),
        ];
        let prompt = build_system_prompt("/repo/pkg", "now", None, &project);
        let root = prompt.find("ROOT").unwrap();
        let pkg = prompt.find("PKG").unwrap();
        assert!(root < pkg);
        assert!(prompt.contains("/repo/AGENTS.md"));
        assert!(prompt.contains("/repo/pkg/AGENTS.md"));
    }

    #[test]
    fn empty_project_rule_is_skipped() {
        let project = vec![
            ("/repo/AGENTS.md".to_string(), "  ".to_string()),
            ("/repo/pkg/AGENTS.md".to_string(), "PKG".to_string()),
        ];
        let prompt = build_system_prompt("/repo/pkg", "now", None, &project);
        assert!(!prompt.contains("/repo/AGENTS.md"));
        assert!(prompt.contains("PKG"));
    }
}

//! System prompt assembly: built-in text, global rules, project rules, then skills.

use crate::skills::Skill;

/// Built-in instructions, compiled in so the binary stays self-contained.
const BUILTIN_PROMPT: &str = include_str!("system_prompt.md");

/// Longest description copied into the prompt; longer ones are cut at a character boundary.
const MAX_DESCRIPTION_CHARS: usize = 100;
/// Most skills listed in one prompt; the rest are summarized in one line.
const MAX_SKILLS: usize = 64;

pub struct Prompt<'a> {
    pub cwd: &'a str,
    pub now: &'a str,
    pub model: &'a str,
    pub global_rules: Option<&'a str>,
    pub project_rules: &'a [(String, String)],
    pub skills: &'a [Skill],
}

pub fn build_system_prompt(input: Prompt<'_>) -> String {
    let mut prompt = BUILTIN_PROMPT
        .replace("{{cwd}}", input.cwd)
        .replace("{{now}}", input.now)
        .replace("{{model}}", input.model);

    if let Some(rules) = input.global_rules {
        let rules = rules.trim();
        if !rules.is_empty() {
            prompt.push_str("\n全局规则：\n");
            prompt.push_str(rules);
            prompt.push('\n');
        }
    }

    for (path, content) in input.project_rules {
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        prompt.push_str(&format!("\n项目规则（{path}）：\n"));
        prompt.push_str(content);
        prompt.push('\n');
    }

    if !input.skills.is_empty() {
        prompt.push_str("\n技能（需要时用 read 打开对应文件）：\n");
        for skill in input.skills.iter().take(MAX_SKILLS) {
            prompt.push_str(&format!("- {}", skill.name));
            prompt.push_str(&format!("（{}）", skill.path.display()));
            let description = skill.description.trim();
            if !description.is_empty() {
                prompt.push('：');
                prompt.push_str(&truncate(description, MAX_DESCRIPTION_CHARS));
            }
            prompt.push('\n');
        }
        let hidden = input.skills.len().saturating_sub(MAX_SKILLS);
        if hidden > 0 {
            prompt.push_str(&format!("另有 {hidden} 个未列出，可用 /skill:<名字> 调用。\n"));
        }
    }

    prompt
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(limit).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            path: format!("/skills/{name}/SKILL.md").into(),
        }
    }

    fn build<'a>(global: Option<&'a str>, project: &'a [(String, String)], skills: &'a [Skill]) -> String {
        build_system_prompt(Prompt {
            cwd: "/tmp",
            now: "now",
            model: "test-model",
            global_rules: global,
            project_rules: project,
            skills,
        })
    }

    #[test]
    fn orders_builtin_then_global_then_project() {
        let project = vec![("/repo/AGENTS.md".to_string(), "PROJECT".to_string())];
        let prompt = build(Some("GLOBAL"), &project, &[]);
        let builtin = prompt.find("你是 li").unwrap();
        let global = prompt.find("GLOBAL").unwrap();
        let found = prompt.find("PROJECT").unwrap();
        assert!(builtin < global);
        assert!(global < found);
    }

    #[test]
    fn skips_missing_rules() {
        let prompt = build(None, &[], &[]);
        assert!(!prompt.contains("GLOBAL"));
        assert!(prompt.ends_with('\n'));
    }

    #[test]
    fn keeps_every_layer_in_outer_to_inner_order() {
        let project = vec![
            ("/repo/AGENTS.md".to_string(), "ROOT".to_string()),
            ("/repo/pkg/AGENTS.md".to_string(), "PKG".to_string()),
        ];
        let prompt = build(None, &project, &[]);
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
        let prompt = build(None, &project, &[]);
        assert!(!prompt.contains("/repo/AGENTS.md"));
        assert!(prompt.contains("PKG"));
    }

    #[test]
    fn names_the_active_model() {
        let prompt = build(None, &[], &[]);
        assert!(prompt.contains("当前模型：test-model"));
    }

    #[test]
    fn lists_skills_after_project_rules_with_their_paths() {
        let project = vec![("/repo/AGENTS.md".to_string(), "PROJECT".to_string())];
        let skills = vec![skill("code-review", "Review the diff")];
        let prompt = build(None, &project, &skills);

        let rules = prompt.find("PROJECT").unwrap();
        let listed = prompt.find("code-review").unwrap();
        assert!(rules < listed);
        assert!(prompt.contains("/skills/code-review/SKILL.md"));
        assert!(prompt.contains("：Review the diff"));
    }

    #[test]
    fn truncates_long_skill_descriptions() {
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 20);
        let skills = vec![skill("verbose", &long)];
        let prompt = build(None, &[], &skills);

        assert!(prompt.contains(&format!("{}…", "x".repeat(MAX_DESCRIPTION_CHARS))));
        assert!(!prompt.contains(&long));
    }

    #[test]
    fn counts_skills_beyond_the_cap() {
        let skills: Vec<Skill> = (0..MAX_SKILLS + 3)
            .map(|index| skill(&format!("s{index:03}"), "d"))
            .collect();
        let prompt = build(None, &[], &skills);

        assert!(prompt.contains("另有 3 个未列出，可用 /skill:<名字> 调用"));
        assert!(prompt.contains("- s063"));
        assert!(!prompt.contains("- s064"));
    }
}

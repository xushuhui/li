//! Skill discovery for the system prompt.
//!
//! Only `name` and `description` from each `SKILL.md` reach the prompt; the body
//! is read on demand with the existing `read` tool. Discovery runs once at
//! startup, so skills added mid-session appear after a restart (D30).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A discovered `SKILL.md`: prompt metadata plus the path the model can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Collects project skills (nearest directory wins) then user skills, sorted by name.
pub fn discover(cwd: &Path, home: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    let mut seen = HashSet::new();

    for dir in project_scope(cwd) {
        collect(&dir.join(".agents").join("skills"), &mut skills, &mut seen);
    }
    collect(&home.join(".agents").join("skills"), &mut skills, &mut seen);

    skills.sort_by(|left, right| left.name.cmp(&right.name));
    skills
}

/// Current directory and its parents, stopping after the directory that holds `.git`.
fn project_scope(cwd: &Path) -> Vec<&Path> {
    let mut dirs = Vec::new();
    for dir in cwd.ancestors() {
        dirs.push(dir);
        if dir.join(".git").exists() {
            break;
        }
    }
    dirs
}

fn collect(root: &Path, skills: &mut Vec<Skill>, seen: &mut HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    dirs.sort();

    for dir in dirs {
        let path = dir.join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let name = frontmatter(&text, "name")
            .or_else(|| dir.file_name().map(|name| name.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let name = name.trim();
        if name.is_empty() || !seen.insert(name.to_string()) {
            continue;
        }
        let description = frontmatter(&text, "description").unwrap_or_default();
        skills.push(Skill {
            name: name.to_string(),
            description,
            path,
        });
    }
}

/// Reads `key: value` from the leading `---` block. Absent or unterminated frontmatter yields `None`.
fn frontmatter(text: &str, key: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            return None;
        }
        let Some((candidate, value)) = line.split_once(':') else {
            continue;
        };
        if candidate.trim() == key {
            return Some(unquote(value.trim()).to_string());
        }
    }
    None
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_skill(root: &Path, dir: &str, body: &str) {
        let dir = root.join(".agents").join("skills").join(dir);
        fs::create_dir_all(&dir).expect("skill dir");
        fs::write(dir.join("SKILL.md"), body).expect("skill file");
    }

    #[test]
    fn reads_name_and_description_from_frontmatter() {
        let home = tempfile::tempdir().expect("home");
        write_skill(
            home.path(),
            "code-review",
            "---\nname: code-review\ndescription: \"Review the diff\"\n---\n\nbody\n",
        );

        let skills = discover(home.path(), home.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "code-review");
        assert_eq!(skills[0].description, "Review the diff");
        assert!(skills[0].path.ends_with("code-review/SKILL.md"));
    }

    #[test]
    fn falls_back_to_directory_name_without_frontmatter() {
        let home = tempfile::tempdir().expect("home");
        write_skill(home.path(), "triage", "no frontmatter here\n");

        let skills = discover(home.path(), home.path());
        assert_eq!(skills[0].name, "triage");
        assert_eq!(skills[0].description, "");
    }

    #[test]
    fn project_skill_shadows_user_skill_of_the_same_name() {
        let home = tempfile::tempdir().expect("home");
        write_skill(home.path(), "review", "---\nname: review\ndescription: USER\n---\n");

        let project = home.path().join("project");
        fs::create_dir_all(project.join(".git")).expect("git dir");
        write_skill(&project, "review", "---\nname: review\ndescription: PROJECT\n---\n");

        let skills = discover(&project, home.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "PROJECT");
    }

    #[test]
    fn walks_up_to_the_repository_root() {
        let home = tempfile::tempdir().expect("home");
        let repo = home.path().join("repo");
        let nested = repo.join("crates/deep");
        fs::create_dir_all(repo.join(".git")).expect("git dir");
        fs::create_dir_all(&nested).expect("nested dir");
        write_skill(&repo, "root-skill", "---\nname: root-skill\ndescription: ROOT\n---\n");

        let skills = discover(&nested, home.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "root-skill");
    }

    #[test]
    fn skips_directories_without_a_skill_file() {
        let home = tempfile::tempdir().expect("home");
        let empty = home.path().join(".agents/skills/empty");
        fs::create_dir_all(&empty).expect("empty dir");

        assert!(discover(home.path(), home.path()).is_empty());
    }
}

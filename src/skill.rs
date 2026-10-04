//! Agent skills bundled with this version of jj-fork, so instructions always match the binary.

use std::path::Path;

use anyhow::{Context, Result};

use crate::{progress, report};

/// `(name, SKILL.md)`.
const SKILLS: &[(&str, &str)] = &[
    (
        "maintaining-forks-with-jj-fork",
        include_str!("../.agents/skills/maintaining-forks-with-jj-fork/SKILL.md"),
    ),
    (
        "setting-up-forks-on-amp",
        include_str!("../.agents/skills/setting-up-forks-on-amp/SKILL.md"),
    ),
];

fn path(root: &Path, name: &str) -> std::path::PathBuf {
    root.join(".agents/skills").join(name).join("SKILL.md")
}

pub fn run(root: &Path, name: Option<&str>, install: bool) -> Result<i32> {
    let selected: Vec<_> = match name {
        Some(name) => vec![SKILLS.iter().find(|(n, _)| *n == name).with_context(|| {
            format!(
                "unknown skill {name}; available: {}",
                SKILLS
                    .iter()
                    .map(|(n, _)| *n)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?],
        None => SKILLS.iter().collect(),
    };
    if install {
        for (name, text) in selected {
            let target = path(root, name);
            std::fs::create_dir_all(target.parent().unwrap())?;
            std::fs::write(&target, text)?;
            progress(&format!("wrote {}", target.display()));
        }
    } else if name.is_some() {
        print!("{}", selected[0].1);
    } else {
        for (name, _) in SKILLS {
            report(name);
        }
    }
    Ok(0)
}

/// Warns when a committed copy of a bundled skill differs from this version's.
pub fn warn_if_stale(root: &Path) {
    for (name, text) in SKILLS {
        if let Ok(committed) = std::fs::read_to_string(path(root, name))
            && committed != *text
        {
            progress(&format!(
                "{} differs from this jj-fork version's skill; refresh it with `jj fork skill {name} --install`",
                path(root, name).display()
            ));
        }
    }
}

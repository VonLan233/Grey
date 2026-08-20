use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

pub const SKILL_FILE: &str = "SKILL.md";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    pub id: String,
    pub path: PathBuf,
}

pub fn validate_id(id: &str) -> Result<()> {
    let valid = (1..=64).contains(&id.len())
        && id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        });
    anyhow::ensure!(
        valid,
        "skill id must match [a-z0-9][a-z0-9_-]{{0,63}}: {id:?}"
    );
    Ok(())
}

pub fn skill_path(config_dir: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(config_dir.join("skills").join(id).join(SKILL_FILE))
}

pub fn index(
    config_dir: &Path,
    skills: impl IntoIterator<Item = (String, bool)>,
) -> Result<Vec<SkillMetadata>> {
    let mut result = Vec::new();
    for (id, enabled) in skills {
        validate_id(&id)?;
        if enabled {
            let path = managed_skill_path(config_dir, &id)?;
            validate_skill_file(&path)?;
            result.push(SkillMetadata { id, path });
        }
    }
    Ok(result)
}

pub fn add_skill(config_dir: &Path, source: &Path, max_bytes: usize) -> Result<SkillMetadata> {
    let source_metadata = fs::symlink_metadata(source)
        .with_context(|| format!("reading skill source metadata {}", source.display()))?;
    anyhow::ensure!(
        source_metadata.is_dir() && !source_metadata.file_type().is_symlink(),
        "skill source must be a non-symlink directory"
    );
    let source = source
        .canonicalize()
        .with_context(|| format!("canonicalizing skill source {}", source.display()))?;
    let source_meta = fs::symlink_metadata(&source)?;
    anyhow::ensure!(
        source_meta.is_dir() && !source_meta.file_type().is_symlink(),
        "skill source must be a non-symlink directory"
    );
    let id = source
        .file_name()
        .and_then(|name| name.to_str())
        .context("skill source directory must have a UTF-8 name")?
        .to_owned();
    validate_id(&id)?;
    let source_file = source.join(SKILL_FILE);
    let content = read_skill_file(&source_file, max_bytes)?;

    let root = config_dir.join("skills");
    fs::create_dir_all(&root).with_context(|| format!("creating skill root {}", root.display()))?;
    anyhow::ensure!(
        !fs::symlink_metadata(&root)?.file_type().is_symlink(),
        "skill root must not be a symlink"
    );
    let target_dir = root.join(&id);
    if target_dir.exists() {
        anyhow::ensure!(
            !fs::symlink_metadata(&target_dir)?.file_type().is_symlink(),
            "skill directory must not be a symlink"
        );
    } else {
        fs::create_dir(&target_dir)
            .with_context(|| format!("creating skill directory {}", target_dir.display()))?;
    }
    let target = target_dir.join(SKILL_FILE);
    anyhow::ensure!(
        !target.exists(),
        "managed skill already exists: {}",
        target.display()
    );
    let mut temporary = NamedTempFile::new_in(&target_dir)
        .with_context(|| format!("creating skill temporary beside {}", target.display()))?;
    use std::io::Write;
    temporary.write_all(content.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&target)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replacing {}", target.display()))?;
    Ok(SkillMetadata { id, path: target })
}

pub fn remove_skill(config_dir: &Path, id: &str) -> Result<()> {
    let path = managed_skill_path(config_dir, id)?;
    validate_skill_file(&path)?;
    fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    let directory = path.parent().expect("skill path has a parent");
    if fs::read_dir(directory)?.next().is_none() {
        fs::remove_dir(directory)
            .with_context(|| format!("removing empty {}", directory.display()))?;
    }
    Ok(())
}

pub fn load_skill(config_dir: &Path, id: &str, max_bytes: usize) -> Result<String> {
    read_skill_file(&managed_skill_path(config_dir, id)?, max_bytes)
}

fn managed_skill_path(config_dir: &Path, id: &str) -> Result<PathBuf> {
    let path = skill_path(config_dir, id)?;
    for directory in [
        config_dir.join("skills"),
        config_dir.join("skills").join(id),
    ] {
        let metadata = fs::symlink_metadata(&directory)
            .with_context(|| format!("reading skill directory metadata {}", directory.display()))?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "skill directory must be a non-symlink directory: {}",
            directory.display()
        );
    }
    Ok(path)
}

pub fn read_skill_file(path: &Path, max_bytes: usize) -> Result<String> {
    validate_skill_file(path)?;
    let file = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut content = Vec::with_capacity(max_bytes.min(64 * 1024).saturating_add(1));
    use std::io::Read;
    file.take((max_bytes.saturating_add(1)) as u64)
        .read_to_end(&mut content)
        .with_context(|| format!("reading {}", path.display()))?;
    anyhow::ensure!(
        content.len() <= max_bytes,
        "skill {} exceeds configured limit of {} bytes",
        path.display(),
        max_bytes
    );
    String::from_utf8(content).with_context(|| format!("skill {} is not UTF-8", path.display()))
}

fn validate_skill_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading skill metadata {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "skill file must be a non-symlink regular file: {}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_ids_and_symlinked_skill_files() {
        assert!(validate_id("reviewer").is_ok());
        assert!(validate_id("1-reviewer").is_ok());
        assert!(validate_id("../reviewer").is_err());
        assert!(validate_id("Reviewer").is_err());
        assert!(validate_id(&"a".repeat(65)).is_err());

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.md");
        std::fs::write(&target, "body").unwrap();
        let link = root.path().join("SKILL.md");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(unix)]
        assert!(read_skill_file(&link, 1024).is_err());
    }

    #[test]
    fn add_and_load_are_config_relative_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("reviewer");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), "# Reviewer\n\nBe precise.").unwrap();
        let config_dir = root.path().join("config");
        std::fs::create_dir(&config_dir).unwrap();

        let added = add_skill(&config_dir, &source, 1024).unwrap();
        assert_eq!(added.id, "reviewer");
        assert_eq!(added.path, config_dir.join("skills/reviewer/SKILL.md"));
        assert_eq!(
            load_skill(&config_dir, "reviewer", 1024).unwrap(),
            "# Reviewer\n\nBe precise."
        );
        assert!(add_skill(&config_dir, &source, 1024).is_err());
        assert!(load_skill(&config_dir, "reviewer", 4).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_managed_directories() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir(outside.join("reviewer")).unwrap();
        std::fs::write(outside.join("reviewer/SKILL.md"), "outside").unwrap();
        let skills = root.path().join("skills");
        std::os::unix::fs::symlink(&outside, &skills).unwrap();
        assert!(load_skill(root.path(), "reviewer", 1024).is_err());
    }
}

use crate::config::{self, PluginConfig, PluginRuntime};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table, Value as TomlValue};

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

pub fn mutation_target() -> Result<PathBuf> {
    if let Some(path) = env::var_os("GREY_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    let project = PathBuf::from("grey.toml");
    if project.exists() {
        return Ok(project);
    }
    match env::var_os("HOME") {
        Some(home) => Ok(PathBuf::from(home).join(".config/grey/grey.toml")),
        None => Ok(project),
    }
}

pub fn edit_file(path: &Path, edit: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let _lock = ConfigLock::acquire(path)?;
    reject_non_regular_file(path)?;
    let source = if path.exists() {
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    let edited = edit_text(&source, edit)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary config beside {}", path.display()))?;
    set_private_permissions(temporary.as_file())?;
    temporary
        .write_all(edited.as_bytes())
        .with_context(|| format!("writing temporary config beside {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("syncing temporary config beside {}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replacing {}", path.display()))?;
    Ok(())
}

pub fn edit_text(
    source: &str,
    edit: impl FnOnce(&mut DocumentMut) -> Result<()>,
) -> Result<String> {
    let mut doc = source
        .parse::<DocumentMut>()
        .context("parsing raw TOML configuration")?;
    edit(&mut doc)?;
    Ok(doc.to_string())
}

pub fn set_enabled(doc: &mut DocumentMut, table: &str, id: &str, enabled: bool) -> Result<()> {
    if table == "plugins" {
        validate_existing_plugins(doc)?;
    }
    let entry = array_of_tables_mut(doc, table)?
        .iter_mut()
        .find(|entry| entry.get("id").and_then(Item::as_str) == Some(id))
        .with_context(|| format!("{table} entry not found: {id}"))?;
    let suffix = entry
        .get("enabled")
        .and_then(Item::as_value)
        .and_then(|value| value.decor().suffix().cloned());
    entry["enabled"] = value(enabled);
    if let Some(suffix) = suffix {
        entry["enabled"]
            .as_value_mut()
            .expect("enabled is a value")
            .decor_mut()
            .set_suffix(suffix);
    }
    Ok(())
}

pub fn upsert_plugin(doc: &mut DocumentMut, plugin: &PluginConfig) -> Result<()> {
    validate_existing_plugins(doc)?;
    let plugins = array_of_tables_mut(doc, "plugins")?;
    let entry = plugins
        .iter_mut()
        .find(|entry| entry.get("id").and_then(Item::as_str) == Some(plugin.id.as_str()));
    let entry = match entry {
        Some(entry) => entry,
        None => {
            let mut entry = Table::new();
            entry["id"] = value(plugin.id.as_str());
            plugins.push(entry);
            plugins
                .get_mut(plugins.len() - 1)
                .expect("new plugin table")
        }
    };
    entry["id"] = value(plugin.id.as_str());
    set_optional_string(entry, "name", plugin.name.as_deref());
    entry["kind"] = value(plugin_kind_name(plugin));
    entry["enabled"] = value(plugin.enabled);
    set_optional_string(entry, "description", plugin.description.as_deref());
    entry["command"] = value(plugin.command.as_str());
    entry["args"] = Item::Value(TomlValue::Array(value_array(&plugin.args)));
    set_optional_integer(entry, "timeout_ms", plugin.timeout_ms);
    set_optional_string(entry, "version", plugin.version.as_deref());
    set_optional_string(entry, "hook_event", plugin.hook_event.as_deref());
    entry["runtime"] = value(plugin_runtime(plugin));
    set_optional_string(entry, "manifest", plugin.manifest.as_deref());
    set_optional_string(entry, "manifest_sha256", plugin.manifest_sha256.as_deref());
    Ok(())
}

pub fn plugin_command(doc: &DocumentMut, id: &str) -> Result<Option<String>> {
    let Some(plugins) = doc
        .as_table()
        .get("plugins")
        .and_then(Item::as_array_of_tables)
    else {
        return Ok(None);
    };
    Ok(plugins
        .iter()
        .find(|entry| entry.get("id").and_then(Item::as_str) == Some(id))
        .and_then(|entry| entry.get("command").and_then(Item::as_str))
        .map(str::to_owned))
}

pub fn plugin_kind_for_id(doc: &DocumentMut, id: &str) -> Result<Option<String>> {
    let Some(plugins) = doc
        .as_table()
        .get("plugins")
        .and_then(Item::as_array_of_tables)
    else {
        return Ok(None);
    };
    Ok(plugins
        .iter()
        .find(|entry| entry.get("id").and_then(Item::as_str) == Some(id))
        .and_then(|entry| entry.get("kind").and_then(Item::as_str))
        .map(str::to_owned))
}

pub fn plugin_config_for_id(doc: &DocumentMut, id: &str) -> Result<Option<PluginConfig>> {
    let Some(plugins) = doc
        .as_table()
        .get("plugins")
        .and_then(Item::as_array_of_tables)
    else {
        return Ok(None);
    };
    plugins
        .iter()
        .find(|entry| entry.get("id").and_then(Item::as_str) == Some(id))
        .map(|entry| toml::from_str(&entry.to_string()).context("parsing raw plugin entry"))
        .transpose()
}

pub fn remove_plugin(doc: &mut DocumentMut, id: &str) -> Result<()> {
    validate_existing_plugins(doc)?;
    let plugins = array_of_tables_mut(doc, "plugins")?;
    let index = plugins
        .iter()
        .position(|entry| entry.get("id").and_then(Item::as_str) == Some(id))
        .with_context(|| format!("plugins entry not found: {id}"))?;
    plugins.remove(index);
    Ok(())
}

/// Updates only the named TUI theme fields, preserving all unrelated raw TOML.
pub fn set_tui_theme(
    doc: &mut DocumentMut,
    preset: &str,
    overrides: &[(&str, &str)],
) -> Result<()> {
    validate_existing_plugins(doc)?;
    let theme = tui_section_mut(doc, "theme")?;
    set_string_field(theme, "preset", preset)?;
    let overrides_section = subsection_mut(theme, "overrides")?;
    for (name, color) in overrides {
        set_string_field(overrides_section, name, color)?;
    }
    Ok(())
}

/// Updates the TUI input height without rewriting other TUI sections.
pub fn set_tui_input_lines(doc: &mut DocumentMut, input_lines: u16) -> Result<()> {
    validate_existing_plugins(doc)?;
    let layout = tui_section_mut(doc, "layout")?;
    set_integer_field(layout, "input_lines", input_lines.into())
}

/// Updates one supported TUI key binding without rewriting the keys table.
pub fn set_tui_key(doc: &mut DocumentMut, name: &str, key: &str) -> Result<()> {
    validate_existing_plugins(doc)?;
    let keys = tui_section_mut(doc, "keys")?;
    set_string_field(keys, name, key)
}

#[derive(Deserialize, Default)]
struct RawPluginList {
    #[serde(default)]
    plugins: Vec<PluginConfig>,
}

fn validate_existing_plugins(doc: &DocumentMut) -> Result<()> {
    let plugins = toml::from_str::<RawPluginList>(&doc.to_string())
        .context("parsing raw plugin configuration")?
        .plugins;
    config::validate_unique_plugin_ids(&plugins)
}

pub fn redact(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(redact),
        Value::Object(values) => values.iter_mut().for_each(|(name, value)| {
            if crate::config::is_secret_field(name) || name == "args" {
                *value = Value::String("***".into());
            } else {
                redact(value);
            }
        }),
        _ => {}
    }
}

fn array_of_tables_mut<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut ArrayOfTables> {
    if !doc.as_table().contains_key(name) {
        doc[name] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    doc[name]
        .as_array_of_tables_mut()
        .with_context(|| format!("{name} must be an array of tables"))
}

fn tui_section_mut<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut Item> {
    let root = doc.as_table_mut();
    if !root.contains_key("tui") {
        root.insert("tui", Item::Table(Table::new()));
    }
    let tui = item_as_table_mut(&mut root["tui"], "tui")?;
    if !tui.contains_key(name) {
        tui.insert(name, Item::Table(Table::new()));
    }
    Ok(tui.get_mut(name).expect("tui section was inserted"))
}

fn set_string_field(section: &mut Item, name: &str, field: &str) -> Result<()> {
    if let Some(table) = section.as_table_mut() {
        table[name] = value(field);
        return Ok(());
    }
    if let Some(table) = section.as_inline_table_mut() {
        table.insert(name, TomlValue::from(field));
        return Ok(());
    }
    bail!("tui section must be a table: {name}")
}

fn set_integer_field(section: &mut Item, name: &str, field: i64) -> Result<()> {
    if let Some(table) = section.as_table_mut() {
        table[name] = value(field);
        return Ok(());
    }
    if let Some(table) = section.as_inline_table_mut() {
        table.insert(name, TomlValue::from(field));
        return Ok(());
    }
    bail!("tui section must be a table: {name}")
}

fn subsection_mut<'a>(section: &'a mut Item, name: &str) -> Result<&'a mut Item> {
    let table = item_as_table_mut(section, "tui section")?;
    if !table.contains_key(name) {
        table.insert(name, Item::Table(Table::new()));
    }
    Ok(table.get_mut(name).expect("tui subsection was inserted"))
}

fn item_as_table_mut<'a>(item: &'a mut Item, name: &str) -> Result<&'a mut Table> {
    if item.as_table().is_none() {
        let inline = item
            .as_inline_table()
            .with_context(|| format!("{name} must be a table"))?;
        let mut table = Table::new();
        for (key, value) in inline.iter() {
            table.insert(key, Item::Value(value.clone()));
        }
        *item = Item::Table(table);
    }
    Ok(item.as_table_mut().expect("item was converted to a table"))
}

fn value_array(values: &[String]) -> Array {
    let mut array = Array::new();
    for argument in values {
        array.push(argument.as_str());
    }
    array
}

fn plugin_kind_name(plugin: &PluginConfig) -> &'static str {
    match plugin.kind {
        crate::config::PluginKind::Tool => "tool",
        crate::config::PluginKind::Provider => "provider",
        crate::config::PluginKind::Hook => "hook",
        crate::config::PluginKind::Theme => "theme",
    }
}

fn plugin_runtime(plugin: &PluginConfig) -> &'static str {
    match plugin.runtime {
        PluginRuntime::Command => "command",
        PluginRuntime::Wasm => "wasm",
    }
}

fn set_optional_string(table: &mut Table, key: &str, field: Option<&str>) {
    match field {
        Some(field) => table[key] = value(field),
        None => {
            table.remove(key);
        }
    }
}

fn set_optional_integer(table: &mut Table, key: &str, field: Option<u64>) {
    match field {
        Some(field) => table[key] = value(field as i64),
        None => {
            table.remove(key);
        }
    }
}

fn reject_non_regular_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing symlink config: {}", path.display())
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            bail!("config is not a regular file: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("checking {}", path.display())),
    }
}

#[cfg(unix)]
fn set_private_permissions(file: &fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .context("setting private config permissions")
}

#[cfg(not(unix))]
fn set_private_permissions(_: &fs::File) -> Result<()> {
    Ok(())
}

struct ConfigLock(fs::File);

impl ConfigLock {
    fn acquire(path: &Path) -> Result<Self> {
        let lock = path.with_file_name(format!(
            "{}.lock",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("grey.toml")
        ));
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock)
                .with_context(|| format!("opening {}", lock.display()))?;
            match file.try_lock() {
                Ok(()) => return Ok(Self(file)),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    bail!("timed out waiting for config lock: {}", lock.display());
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(error).with_context(|| format!("locking {}", lock.display()))
                }
            }
        }
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_config_edit_preserves_comments_placeholders_and_unknown_fields() {
        let source =
            "# keep\n[[plugins]]\nid = \"old\"\nargs = [\"${PLUGIN_TOKEN}\"]\nextra = \"keep\"\n";
        let edited = edit_text(source, |doc| set_enabled(doc, "plugins", "old", false)).unwrap();
        assert!(edited.contains("# keep"));
        assert!(edited.contains("${PLUGIN_TOKEN}"));
        assert!(edited.contains("extra = \"keep\""));
        assert!(edited.contains("enabled = false"));
    }

    #[test]
    fn raw_config_edit_preserves_enabled_comment() {
        let edited = edit_text(
            "[[plugins]]\nid = \"old\"\nenabled = true # retain-me\n",
            |doc| set_enabled(doc, "plugins", "old", false),
        )
        .unwrap();
        assert!(edited.contains("enabled = false # retain-me"));
    }

    #[test]
    fn raw_config_redacts_nested_secret_fields() {
        let mut value = serde_json::json!({"nested": {"authorization": "hidden"}});
        redact(&mut value);
        assert_eq!(value["nested"]["authorization"], "***");
    }

    #[test]
    fn raw_config_upsert_preserves_unknown_wasm_plugin_fields() {
        let source = "# keep\n[[plugins]]\nid = \"echo\"\nowner = \"user\"\n";
        let edited = edit_text(source, |doc| {
            upsert_plugin(
                doc,
                &PluginConfig {
                    id: "echo".into(),
                    kind: crate::config::PluginKind::Tool,
                    enabled: true,
                    runtime: PluginRuntime::Wasm,
                    manifest: Some("plugins/echo/plugin.json".into()),
                    manifest_sha256: Some("0".repeat(64)),
                    ..Default::default()
                },
            )
        })
        .unwrap();
        assert!(edited.contains("# keep"));
        assert!(edited.contains("owner = \"user\""));
        assert!(edited.contains("runtime = \"wasm\""));
        assert!(edited.contains("manifest = \"plugins/echo/plugin.json\""));
        assert!(edited.contains("manifest_sha256"));
    }

    #[test]
    fn tui_edits_preserve_unknown_inline_fields_and_placeholders() {
        let source = "# keep\nunknown = \"${KEEP}\"\n[tui]\nowner = \"user\"\ntheme = { preset = \"slate\", custom = \"keep\" }\n";
        let edited = edit_text(source, |doc| {
            set_tui_theme(doc, "grey_storm", &[("accent", "#44e0d3")])?;
            set_tui_input_lines(doc, 6)?;
            set_tui_key(doc, "help", "ctrl-h")
        })
        .unwrap();
        assert!(edited.contains("# keep"));
        assert!(edited.contains("${KEEP}"));
        assert!(edited.contains("owner = \"user\""));
        assert!(edited.contains("custom = \"keep\""));
        assert!(edited.contains("preset = \"grey_storm\""));
        assert!(edited.contains("accent = \"#44e0d3\""));
        assert!(edited.contains("input_lines = 6"));
        assert!(edited.contains("help = \"ctrl-h\""));
    }

    #[test]
    fn tui_edits_reject_duplicate_plugin_ids() {
        let source = "[[plugins]]\nid = \"duplicate\"\n[[plugins]]\nid = \"duplicate\"\n";
        assert!(edit_text(source, |doc| set_tui_key(doc, "help", "h")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn raw_config_new_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("grey.toml");
        edit_file(&path, |doc| {
            upsert_plugin(
                doc,
                &PluginConfig {
                    id: "private".into(),
                    command: "printf".into(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    }
}

//! Sealed WASI Preview1 runtime for explicit WebAssembly plugin manifests.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use serde::Deserialize;
use wasmtime::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{
    p1::{self, WasiP1Ctx},
    p2::pipe::{MemoryInputPipe, MemoryOutputPipe},
    WasiCtxBuilder,
};

use crate::{
    config::validate_plugin_config, PluginConfig, PluginKind, PluginRuntime, RuntimeConfig,
};

pub const WASM_PLUGIN_PROTOCOL: &str = "grey.wasm-plugin.v1";
pub const WASM_PLUGIN_SCHEMA_VERSION: u32 = 1;
const DEFAULT_WASM_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmPluginErrorKind {
    Config,
    Manifest,
    Runtime,
}

#[derive(Debug, Clone)]
pub struct WasmPluginError {
    kind: WasmPluginErrorKind,
    message: String,
}

impl WasmPluginError {
    fn new(kind: WasmPluginErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> WasmPluginErrorKind {
        self.kind
    }
}

impl fmt::Display for WasmPluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WasmPluginError {}

#[derive(Debug, Clone)]
pub struct WasmPluginOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct WasmPlugin {
    id: String,
    kind: PluginKind,
    module: PathBuf,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    stdin_limit: usize,
    memory_bytes: usize,
    fuel: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WasmPluginManifest {
    schema_version: u32,
    id: String,
    kind: PluginKind,
    protocol: String,
    wasi: String,
    module: String,
}

struct WasiState {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
}

impl WasmPlugin {
    pub fn from_config(
        plugin: &PluginConfig,
        config_dir: &Path,
        runtime: &RuntimeConfig,
    ) -> Result<Self, WasmPluginError> {
        validate_plugin_config(plugin).map_err(|_| {
            WasmPluginError::new(
                WasmPluginErrorKind::Config,
                "invalid wasm plugin configuration",
            )
        })?;
        if plugin.runtime != PluginRuntime::Wasm {
            return Err(WasmPluginError::new(
                WasmPluginErrorKind::Config,
                "plugin is not configured for the wasm runtime",
            ));
        }

        let config_dir = regular_directory(config_dir)?;
        let manifest_path = resolve_relative_file(
            &config_dir,
            plugin.manifest.as_deref().expect("validated wasm manifest"),
        )?;
        let manifest = parse_manifest(&manifest_path)?;
        if manifest.schema_version != WASM_PLUGIN_SCHEMA_VERSION
            || manifest.id != plugin.id
            || manifest.kind != plugin.kind
            || manifest.protocol != WASM_PLUGIN_PROTOCOL
            || manifest.wasi != "preview1-stdio"
        {
            return Err(WasmPluginError::new(
                WasmPluginErrorKind::Manifest,
                "wasm plugin manifest does not match its sealed configuration",
            ));
        }
        let module_base = manifest_path.parent().ok_or_else(|| {
            WasmPluginError::new(
                WasmPluginErrorKind::Manifest,
                "invalid wasm plugin manifest",
            )
        })?;
        let module = resolve_relative_file(module_base, &manifest.module)?;
        let timeout =
            Duration::from_millis(plugin.timeout_ms.unwrap_or(
                u64::try_from(DEFAULT_WASM_TIMEOUT.as_millis()).expect("timeout fits u64"),
            ));
        if timeout.is_zero() {
            return Err(WasmPluginError::new(
                WasmPluginErrorKind::Config,
                "wasm plugin timeout must be greater than zero",
            ));
        }
        Ok(Self {
            id: plugin.id.clone(),
            kind: plugin.kind,
            module,
            timeout,
            stdout_limit: runtime.command_stdout_max_bytes,
            stderr_limit: runtime.command_stderr_max_bytes,
            stdin_limit: runtime
                .response_max_bytes
                .min(crate::process::DEFAULT_STDIN_LIMIT),
            memory_bytes: runtime.wasm_memory_bytes,
            fuel: runtime.wasm_fuel,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn kind(&self) -> PluginKind {
        self.kind
    }

    pub async fn invoke(&self, input: Vec<u8>) -> Result<WasmPluginOutput, WasmPluginError> {
        if input.len() > self.stdin_limit {
            return Err(WasmPluginError::new(
                WasmPluginErrorKind::Runtime,
                "wasm plugin input exceeds configured limit",
            ));
        }
        let module = self.module.clone();
        let timeout = self.timeout;
        let stdout_limit = self.stdout_limit;
        let stderr_limit = self.stderr_limit;
        let memory_bytes = self.memory_bytes;
        let fuel = self.fuel;
        tokio::task::spawn_blocking(move || {
            invoke_blocking(
                module,
                input,
                timeout,
                stdout_limit,
                stderr_limit,
                memory_bytes,
                fuel,
            )
        })
        .await
        .map_err(|_| {
            WasmPluginError::new(WasmPluginErrorKind::Runtime, "wasm plugin task failed")
        })?
    }
}

fn regular_directory(path: &Path) -> Result<PathBuf, WasmPluginError> {
    let path = path.canonicalize().map_err(|_| {
        WasmPluginError::new(
            WasmPluginErrorKind::Config,
            "invalid plugin configuration directory",
        )
    })?;
    if !path.is_dir() {
        return Err(WasmPluginError::new(
            WasmPluginErrorKind::Config,
            "invalid plugin configuration directory",
        ));
    }
    Ok(path)
}

fn resolve_relative_file(base: &Path, relative: &str) -> Result<PathBuf, WasmPluginError> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        return Err(WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            "wasm plugin paths must be non-empty and relative",
        ));
    }
    let joined = base.join(relative);
    let metadata = fs::symlink_metadata(&joined).map_err(|_| {
        WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            "wasm plugin file is unavailable",
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            "wasm plugin file must be a regular non-symlink file",
        ));
    }
    let path = joined.canonicalize().map_err(|_| {
        WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            "wasm plugin file is unavailable",
        )
    })?;
    if !path.starts_with(base) {
        return Err(WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            "wasm plugin file escapes its allowed directory",
        ));
    }
    Ok(path)
}

fn parse_manifest(path: &Path) -> Result<WasmPluginManifest, WasmPluginError> {
    let bytes = fs::read(path).map_err(|_| {
        WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            "could not read wasm plugin manifest",
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            "invalid wasm plugin manifest",
        )
    })
}

fn invoke_blocking(
    module_path: PathBuf,
    input: Vec<u8>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    memory_bytes: usize,
    fuel: u64,
) -> Result<WasmPluginOutput, WasmPluginError> {
    let mut config = Config::new();
    config.consume_fuel(true);
    config.epoch_interruption(true);
    let engine = Engine::new(&config).map_err(|_| {
        WasmPluginError::new(
            WasmPluginErrorKind::Runtime,
            "could not initialize wasm runtime",
        )
    })?;
    let module = Module::from_file(&engine, &module_path).map_err(|_| {
        WasmPluginError::new(
            WasmPluginErrorKind::Runtime,
            "could not load wasm plugin module",
        )
    })?;
    let stdout = MemoryOutputPipe::new(stdout_limit);
    let stderr = MemoryOutputPipe::new(stderr_limit);
    let mut builder = WasiCtxBuilder::new();
    builder
        .stdin(MemoryInputPipe::new(input))
        .stdout(stdout.clone())
        .stderr(stderr.clone());
    let state = WasiState {
        wasi: builder.build_p1(),
        limits: StoreLimitsBuilder::new()
            .memory_size(memory_bytes)
            .instances(1)
            .tables(16)
            .memories(1)
            .trap_on_grow_failure(true)
            .build(),
    };
    let mut store = Store::new(&engine, state);
    store.limiter(|state| &mut state.limits);
    store.set_fuel(fuel).map_err(|_| {
        WasmPluginError::new(
            WasmPluginErrorKind::Runtime,
            "could not set wasm fuel limit",
        )
    })?;
    store.set_epoch_deadline(1);
    store.epoch_deadline_trap();
    let (done_tx, done_rx) = mpsc::channel();
    let timer_engine = engine.clone();
    let timer = thread::spawn(move || {
        if done_rx.recv_timeout(timeout).is_err() {
            timer_engine.increment_epoch();
        }
    });
    let result = (|| {
        let mut linker = Linker::new(&engine);
        p1::add_to_linker_sync(&mut linker, |state: &mut WasiState| &mut state.wasi).map_err(
            |_| WasmPluginError::new(WasmPluginErrorKind::Runtime, "could not link wasm plugin"),
        )?;
        let instance = linker.instantiate(&mut store, &module).map_err(|_| {
            WasmPluginError::new(WasmPluginErrorKind::Runtime, "could not start wasm plugin")
        })?;
        let start = instance
            .get_typed_func::<(), ()>(&mut store, "_start")
            .map_err(|_| {
                WasmPluginError::new(
                    WasmPluginErrorKind::Runtime,
                    "wasm plugin has no _start entrypoint",
                )
            })?;
        start.call(&mut store, ()).map_err(|_| {
            WasmPluginError::new(WasmPluginErrorKind::Runtime, "wasm plugin execution failed")
        })
    })();
    let _ = done_tx.send(());
    let _ = timer.join();
    result?;
    Ok(WasmPluginOutput {
        stdout: stdout.contents().to_vec(),
        stderr: stderr.contents().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginConfig;
    use tempfile::TempDir;

    fn plugin(manifest: &str) -> PluginConfig {
        PluginConfig {
            id: "echo".into(),
            kind: PluginKind::Tool,
            enabled: true,
            runtime: PluginRuntime::Wasm,
            manifest: Some(manifest.into()),
            ..Default::default()
        }
    }

    #[test]
    fn wasm_configuration_is_strict_and_defaults_to_command() {
        let config: crate::GreyConfig = toml::from_str(
            "[[plugins]]\nid = \"echo\"\nkind = \"tool\"\nruntime = \"wasm\"\nmanifest = \"plugins/echo/plugin.json\"\n",
        )
        .unwrap();
        assert_eq!(config.plugins[0].runtime, PluginRuntime::Wasm);
        assert_eq!(
            config.plugins[0].manifest.as_deref(),
            Some("plugins/echo/plugin.json")
        );

        let mut plugin = plugin("plugin.json");
        assert!(validate_plugin_config(&plugin).is_ok());
        plugin.command = "runner".into();
        assert!(validate_plugin_config(&plugin).is_err());
        plugin.command.clear();
        plugin.kind = PluginKind::Hook;
        assert!(validate_plugin_config(&plugin).is_err());
        plugin.kind = PluginKind::Tool;
        plugin.manifest = None;
        assert!(validate_plugin_config(&plugin).is_err());
    }

    fn write_fixture(wat_source: &str) -> TempDir {
        let root = tempfile::tempdir().unwrap();
        let plugins = root.path().join("plugins/echo");
        fs::create_dir_all(&plugins).unwrap();
        fs::write(
            plugins.join("module.wasm"),
            wat::parse_str(wat_source).unwrap(),
        )
        .unwrap();
        fs::write(
            plugins.join("plugin.json"),
            r#"{"schema_version":1,"id":"echo","kind":"tool","protocol":"grey.wasm-plugin.v1","wasi":"preview1-stdio","module":"module.wasm"}"#,
        )
        .unwrap();
        root
    }

    const ECHO: &str = r#"
        (module
          (import "wasi_snapshot_preview1" "fd_read" (func $read (param i32 i32 i32 i32) (result i32)))
          (import "wasi_snapshot_preview1" "fd_write" (func $write (param i32 i32 i32 i32) (result i32)))
          (memory 1)
          (export "memory" (memory 0))
          (data (i32.const 0) "\10\00\00\00\00\04\00\00")
          (func (export "_start") (local $n i32)
            i32.const 0 i32.const 0 i32.const 1 i32.const 8 call $read drop
            i32.const 8 i32.load local.set $n
            i32.const 4 local.get $n i32.store
            i32.const 1 i32.const 0 i32.const 1 i32.const 12 call $write drop))
    "#;

    #[tokio::test]
    async fn stdio_only_wasm_plugin_echoes_json_without_host_capabilities() {
        let root = write_fixture(ECHO);
        let runtime = RuntimeConfig::default();
        let wasm =
            WasmPlugin::from_config(&plugin("plugins/echo/plugin.json"), root.path(), &runtime)
                .unwrap();
        let output = wasm.invoke(br#"{"request":"ok"}"#.to_vec()).await.unwrap();
        assert_eq!(output.stdout, br#"{"request":"ok"}"#);
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn manifest_rejects_unknown_fields_and_path_escape() {
        let root = write_fixture(ECHO);
        let manifest = root.path().join("plugins/echo/plugin.json");
        fs::write(
            &manifest,
            r#"{"schema_version":1,"id":"echo","kind":"tool","protocol":"grey.wasm-plugin.v1","wasi":"preview1-stdio","module":"../module.wasm","extra":true}"#,
        )
        .unwrap();
        let error = WasmPlugin::from_config(
            &plugin("plugins/echo/plugin.json"),
            root.path(),
            &RuntimeConfig::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), WasmPluginErrorKind::Manifest);

        let error = WasmPlugin::from_config(
            &plugin("../plugin.json"),
            root.path(),
            &RuntimeConfig::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), WasmPluginErrorKind::Manifest);
    }

    #[cfg(unix)]
    #[test]
    fn manifest_rejects_symlinked_plugin_files() {
        use std::os::unix::fs::symlink;

        let root = write_fixture(ECHO);
        let outside = root.path().join("outside.json");
        fs::write(
            &outside,
            r#"{"schema_version":1,"id":"echo","kind":"tool","protocol":"grey.wasm-plugin.v1","wasi":"preview1-stdio","module":"module.wasm"}"#,
        )
        .unwrap();
        symlink(&outside, root.path().join("plugins/echo/linked.json")).unwrap();
        let error = WasmPlugin::from_config(
            &plugin("plugins/echo/linked.json"),
            root.path(),
            &RuntimeConfig::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), WasmPluginErrorKind::Manifest);
    }

    #[tokio::test]
    async fn memory_growth_and_loop_are_bounded() {
        let grow = write_fixture(
            r#"(module (memory 1) (func (export "_start") i32.const 1025 memory.grow drop))"#,
        );
        let wasm = WasmPlugin::from_config(
            &plugin("plugins/echo/plugin.json"),
            grow.path(),
            &RuntimeConfig::default(),
        )
        .unwrap();
        assert_eq!(
            wasm.invoke(Vec::new()).await.unwrap_err().kind(),
            WasmPluginErrorKind::Runtime
        );

        let looped = write_fixture(r#"(module (func (export "_start") (loop br 0)))"#);
        let mut loop_plugin = plugin("plugins/echo/plugin.json");
        loop_plugin.timeout_ms = Some(1);
        let wasm = WasmPlugin::from_config(&loop_plugin, looped.path(), &RuntimeConfig::default())
            .unwrap();
        assert_eq!(
            wasm.invoke(Vec::new()).await.unwrap_err().kind(),
            WasmPluginErrorKind::Runtime
        );
    }
}

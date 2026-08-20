//! Sealed WASI Preview1 runtime for explicit WebAssembly plugin manifests.

use std::{
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use sha2::{Digest, Sha256};
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
const MAX_WASM_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_MODULE_BYTES: u64 = 4 * 1024 * 1024;

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

#[derive(Clone)]
pub struct WasmPluginOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl fmt::Debug for WasmPluginOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmPluginOutput")
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .field("stdout_truncated", &self.stdout_truncated)
            .field("stderr_truncated", &self.stderr_truncated)
            .finish()
    }
}

#[derive(Clone)]
pub struct WasmPlugin {
    id: String,
    kind: PluginKind,
    engine: Engine,
    module: Module,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    stdin_limit: usize,
    memory_bytes: usize,
    fuel: u64,
}

impl fmt::Debug for WasmPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmPlugin")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("timeout", &self.timeout)
            .field("stdout_limit", &self.stdout_limit)
            .field("stderr_limit", &self.stderr_limit)
            .field("stdin_limit", &self.stdin_limit)
            .field("memory_bytes", &self.memory_bytes)
            .field("fuel", &self.fuel)
            .finish()
    }
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
    module_sha256: String,
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
        let runtime = normalized_runtime(runtime);

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
        let module_path = resolve_relative_file(module_base, &manifest.module)?;
        let module_bytes = read_bounded_file(&module_path, MAX_MODULE_BYTES, "wasm plugin module")?;
        verify_module_hash(&module_bytes, &manifest.module_sha256)?;
        let timeout =
            Duration::from_millis(plugin.timeout_ms.unwrap_or(
                u64::try_from(DEFAULT_WASM_TIMEOUT.as_millis()).expect("timeout fits u64"),
            ))
            .min(MAX_WASM_TIMEOUT);
        if timeout.is_zero() {
            return Err(WasmPluginError::new(
                WasmPluginErrorKind::Config,
                "wasm plugin timeout must be greater than zero",
            ));
        }
        let mut config = Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|_| {
            WasmPluginError::new(
                WasmPluginErrorKind::Runtime,
                "could not initialize wasm runtime",
            )
        })?;
        // ponytail: Wasmtime has no cancellable compile API; this 4MiB hash-pinned local asset bounds startup input/integrity. Add a multi-process compiler only for untrusted remote installation.
        let module = Module::from_binary(&engine, &module_bytes).map_err(|_| {
            WasmPluginError::new(
                WasmPluginErrorKind::Runtime,
                "could not compile wasm plugin module",
            )
        })?;
        Ok(Self {
            id: plugin.id.clone(),
            kind: plugin.kind,
            module,
            engine,
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
        let ticker = EpochTicker::start(self.engine.clone());
        let result = tokio::time::timeout(
            self.timeout,
            invoke_async(
                self.engine.clone(),
                self.module.clone(),
                input,
                self.stdout_limit,
                self.stderr_limit,
                self.memory_bytes,
                self.fuel,
            ),
        )
        .await;
        drop(ticker);
        result.map_err(|_| {
            WasmPluginError::new(
                WasmPluginErrorKind::Runtime,
                "wasm plugin execution timed out",
            )
        })?
    }
}

fn normalized_runtime(runtime: &RuntimeConfig) -> RuntimeConfig {
    runtime.normalized()
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
    let bytes = read_bounded_file(path, MAX_MANIFEST_BYTES, "wasm plugin manifest")?;
    serde_json::from_slice(&bytes).map_err(|_| {
        WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            "invalid wasm plugin manifest",
        )
    })
}

fn verify_module_hash(bytes: &[u8], expected: &str) -> Result<(), WasmPluginError> {
    let expected = hex::decode(expected).map_err(|_| {
        WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            "invalid wasm plugin module hash",
        )
    })?;
    if expected.len() != 32 || expected.as_slice() != Sha256::digest(bytes).as_slice() {
        return Err(WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            "wasm plugin module hash does not match manifest",
        ));
    }
    Ok(())
}

fn read_bounded_file(path: &Path, max_bytes: u64, label: &str) -> Result<Vec<u8>, WasmPluginError> {
    let mut file = fs::File::open(path).map_err(|_| {
        WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            format!("could not open {label}"),
        )
    })?;
    let metadata = file.metadata().map_err(|_| {
        WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            format!("could not inspect {label}"),
        )
    })?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            format!("{label} exceeds the sealed size limit"),
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| {
            WasmPluginError::new(
                WasmPluginErrorKind::Manifest,
                format!("could not read {label}"),
            )
        })?;
    if bytes.len() as u64 > max_bytes {
        return Err(WasmPluginError::new(
            WasmPluginErrorKind::Manifest,
            format!("{label} exceeds the sealed size limit"),
        ));
    }
    Ok(bytes)
}

struct EpochTicker(Option<tokio::sync::oneshot::Sender<()>>);

impl EpochTicker {
    fn start(engine: Engine) -> Self {
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(1)) => engine.increment_epoch(),
                    _ = &mut stopped => return,
                }
            }
        });
        Self(Some(stop))
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        if let Some(stop) = self.0.take() {
            let _ = stop.send(());
        }
    }
}

async fn invoke_async(
    engine: Engine,
    module: Module,
    input: Vec<u8>,
    stdout_limit: usize,
    stderr_limit: usize,
    memory_bytes: usize,
    fuel: u64,
) -> Result<WasmPluginOutput, WasmPluginError> {
    let stdout = MemoryOutputPipe::new(stdout_limit.saturating_add(1));
    let stderr = MemoryOutputPipe::new(stderr_limit.saturating_add(1));
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
    store.epoch_deadline_async_yield_and_update(1);
    let mut linker = Linker::new(&engine);
    p1::add_to_linker_async(&mut linker, |state: &mut WasiState| &mut state.wasi).map_err(
        |_| WasmPluginError::new(WasmPluginErrorKind::Runtime, "could not link wasm plugin"),
    )?;
    let instance = linker
        .instantiate_async(&mut store, &module)
        .await
        .map_err(|_| {
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
    start.call_async(&mut store, ()).await.map_err(|_| {
        WasmPluginError::new(WasmPluginErrorKind::Runtime, "wasm plugin execution failed")
    })?;
    let stdout = stdout.contents().to_vec();
    let stderr = stderr.contents().to_vec();
    if stdout.len() > stdout_limit || stderr.len() > stderr_limit {
        return Err(WasmPluginError::new(
            WasmPluginErrorKind::Runtime,
            "wasm plugin output exceeds configured limit",
        ));
    }
    // A full pipe is conservatively marked truncated because Preview1 fd_write may short-write.
    Ok(WasmPluginOutput {
        stdout_truncated: stdout.len() == stdout_limit,
        stderr_truncated: stderr.len() == stderr_limit,
        stdout,
        stderr,
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
        let module = wat::parse_str(wat_source).unwrap();
        fs::write(plugins.join("module.wasm"), &module).unwrap();
        fs::write(
            plugins.join("plugin.json"),
            format!(
                r#"{{"schema_version":1,"id":"echo","kind":"tool","protocol":"grey.wasm-plugin.v1","wasi":"preview1-stdio","module":"module.wasm","module_sha256":"{}"}}"#,
                hex::encode(Sha256::digest(&module))
            ),
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

    #[test]
    fn manifest_rejects_each_identity_contract_mismatch() {
        let root = write_fixture(ECHO);
        let manifest = root.path().join("plugins/echo/plugin.json");
        for value in [
            serde_json::json!({"schema_version":2,"id":"echo","kind":"tool","protocol":WASM_PLUGIN_PROTOCOL,"wasi":"preview1-stdio","module":"module.wasm"}),
            serde_json::json!({"schema_version":1,"id":"other","kind":"tool","protocol":WASM_PLUGIN_PROTOCOL,"wasi":"preview1-stdio","module":"module.wasm"}),
            serde_json::json!({"schema_version":1,"id":"echo","kind":"theme","protocol":WASM_PLUGIN_PROTOCOL,"wasi":"preview1-stdio","module":"module.wasm"}),
            serde_json::json!({"schema_version":1,"id":"echo","kind":"tool","protocol":"other","wasi":"preview1-stdio","module":"module.wasm"}),
            serde_json::json!({"schema_version":1,"id":"echo","kind":"tool","protocol":WASM_PLUGIN_PROTOCOL,"wasi":"preview2","module":"module.wasm"}),
        ] {
            fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
            assert_eq!(
                WasmPlugin::from_config(
                    &plugin("plugins/echo/plugin.json"),
                    root.path(),
                    &RuntimeConfig::default(),
                )
                .unwrap_err()
                .kind(),
                WasmPluginErrorKind::Manifest
            );
        }
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

    #[test]
    fn bounded_loader_rejects_oversized_manifest_and_module() {
        let root = write_fixture(ECHO);
        let manifest = root.path().join("plugins/echo/plugin.json");
        fs::write(&manifest, vec![b'x'; (MAX_MANIFEST_BYTES + 1) as usize]).unwrap();
        assert_eq!(
            WasmPlugin::from_config(
                &plugin("plugins/echo/plugin.json"),
                root.path(),
                &RuntimeConfig::default(),
            )
            .unwrap_err()
            .kind(),
            WasmPluginErrorKind::Manifest
        );

        let module = root.path().join("plugins/echo/module.wasm");
        fs::write(&module, b"too large").unwrap();
        assert_eq!(
            read_bounded_file(&module, 1, "wasm plugin module")
                .unwrap_err()
                .kind(),
            WasmPluginErrorKind::Manifest
        );
    }

    #[test]
    fn module_content_swap_or_hash_mismatch_is_rejected_before_compilation() {
        let root = write_fixture(ECHO);
        let module = root.path().join("plugins/echo/module.wasm");
        fs::write(&module, wat::parse_str("(module)").unwrap()).unwrap();
        assert_eq!(
            WasmPlugin::from_config(
                &plugin("plugins/echo/plugin.json"),
                root.path(),
                &RuntimeConfig::default(),
            )
            .unwrap_err()
            .kind(),
            WasmPluginErrorKind::Manifest
        );
    }

    #[tokio::test]
    async fn dropping_an_infinite_invocation_cancels_without_waiting_for_plugin_timeout() {
        let looped = write_fixture(r#"(module (func (export "_start") (loop br 0)))"#);
        let mut loop_plugin = plugin("plugins/echo/plugin.json");
        loop_plugin.timeout_ms = Some(60_000);
        let wasm = WasmPlugin::from_config(&loop_plugin, looped.path(), &RuntimeConfig::default())
            .unwrap();
        let task = tokio::spawn(async move { wasm.invoke(Vec::new()).await });
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn runtime_limits_and_stdio_output_cap_cannot_be_bypassed_by_handbuilt_config() {
        let root = write_fixture(ECHO);
        let runtime = RuntimeConfig {
            response_max_bytes: 0,
            command_stdout_max_bytes: usize::MAX,
            command_stderr_max_bytes: 0,
            wasm_memory_bytes: usize::MAX,
            wasm_fuel: u64::MAX,
            ..RuntimeConfig::default()
        };
        let wasm =
            WasmPlugin::from_config(&plugin("plugins/echo/plugin.json"), root.path(), &runtime)
                .unwrap();
        assert_eq!(wasm.memory_bytes, crate::config::RUNTIME_WASM_MEMORY_MAX);
        assert_eq!(wasm.fuel, crate::config::RUNTIME_WASM_FUEL_MAX);
        assert_eq!(wasm.stdout_limit, 64 * 1024 * 1024);
        assert_eq!(wasm.stderr_limit, 1024);
        assert_eq!(wasm.stdin_limit, 1024);
        assert_eq!(
            wasm.invoke(vec![b'x'; 1025]).await.unwrap_err().kind(),
            WasmPluginErrorKind::Runtime
        );
    }

    #[tokio::test]
    async fn stdio_and_entrypoint_failures_are_bounded() {
        let root = write_fixture(ECHO);
        let runtime = RuntimeConfig {
            response_max_bytes: 2_048,
            command_stdout_max_bytes: 1_024,
            ..RuntimeConfig::default()
        };
        let wasm =
            WasmPlugin::from_config(&plugin("plugins/echo/plugin.json"), root.path(), &runtime)
                .unwrap();
        let output = wasm.invoke(vec![b'x'; 1_025]).await.unwrap();
        assert_eq!(output.stdout.len(), 1_024);
        assert!(output.stdout_truncated);

        let no_start = write_fixture("(module)");
        let wasm = WasmPlugin::from_config(
            &plugin("plugins/echo/plugin.json"),
            no_start.path(),
            &RuntimeConfig::default(),
        )
        .unwrap();
        assert_eq!(
            wasm.invoke(Vec::new()).await.unwrap_err().kind(),
            WasmPluginErrorKind::Runtime
        );
    }

    #[tokio::test]
    async fn wasi_runtime_exposes_no_arguments_environment_or_preopened_directories() {
        let root = write_fixture(
            r#"(module
                (import "wasi_snapshot_preview1" "args_sizes_get" (func $args (param i32 i32) (result i32)))
                (import "wasi_snapshot_preview1" "environ_sizes_get" (func $env (param i32 i32) (result i32)))
                (import "wasi_snapshot_preview1" "fd_prestat_get" (func $prestat (param i32 i32) (result i32)))
                (import "wasi_snapshot_preview1" "fd_write" (func $write (param i32 i32 i32 i32) (result i32)))
                (memory 1) (export "memory" (memory 0))
                (func (export "_start")
                  i32.const 0 i32.const 4 call $args drop
                  i32.const 8 i32.const 12 call $env drop
                  i32.const 16 i32.const 3 i32.const 20 call $prestat i32.store
                  i32.const 32 i32.const 0 i32.store
                  i32.const 36 i32.const 20 i32.store
                  i32.const 1 i32.const 32 i32.const 1 i32.const 40 call $write drop))"#,
        );
        let wasm = WasmPlugin::from_config(
            &plugin("plugins/echo/plugin.json"),
            root.path(),
            &RuntimeConfig::default(),
        )
        .unwrap();
        let output = wasm.invoke(Vec::new()).await.unwrap();
        assert_eq!(&output.stdout[..16], [0; 16]);
        assert_ne!(
            u32::from_le_bytes(output.stdout[16..20].try_into().unwrap()),
            0
        );
    }
}

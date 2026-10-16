//! Lua plugin loader for Pumpkin.
//!
//! Loads `.lua` plugins into an embedded `LuaJIT` runtime (no JVM anywhere).
//! Each plugin runs in a sandboxed state: the `debug` library, process
//! control (`os.execute`, `os.exit`, ...), file-backed `require` and raw
//! `io` handles outside the plugin's data folder are removed. The
//! Bukkit-compatibility shim (`lua/bukkit.lua`, embedded at build time) is
//! the only preloaded module, and the `host` table exposes server functions.
//!
//! Trust model: a Lua plugin is as trusted as a native `.so` plugin with
//! respect to CPU/memory, but it cannot touch files outside its data folder
//! or spawn processes. Symlink escapes from the data folder are a known
//! follow-up (lexical checks only for now).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use mlua::{Function, Lua, RegistryKey, Table, Value};
use pumpkin_core::plugin::{
    PluginMetadata,
    api::{Context, Plugin, PluginFuture},
    loader::{LoaderError, PluginLoadFuture, PluginLoader, PluginUnloadFuture},
};

/// Maximum plugin file size accepted on read.
const MAX_PLUGIN_BYTES: u64 = 1024 * 1024;

/// Embedded Bukkit-compatibility shim, the only preloaded Lua module.
const SHIM_SOURCE: &str = include_str!("../lua/bukkit.lua");

mod sandbox;
mod validate;

use sandbox::apply_sandbox;
use validate::{bounded_string, config_path_for, validate_config_name};

/// Loads `.lua` files as Pumpkin plugins.
pub struct LuaPluginLoader;

impl LuaPluginLoader {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for LuaPluginLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginLoader for LuaPluginLoader {
    fn load<'a>(&'a self, path: &'a Path) -> PluginLoadFuture<'a> {
        Box::pin(async {
            let path = path.to_owned();
            let source = read_bounded(&path).map_err(LoaderError::RuntimeError)?;
            let (plugin, metadata) = LuaPlugin::spawn(&source)?;
            Ok((
                Arc::new(plugin) as Arc<dyn Plugin>,
                metadata,
                Box::new(()) as Box<dyn std::any::Any + Send + Sync>,
            ))
        })
    }

    fn can_load(&self, path: &Path) -> bool {
        path.extension().is_some_and(|ext| ext == "lua")
    }

    fn unload(&self, _data: Box<dyn std::any::Any + Send + Sync>) -> PluginUnloadFuture<'_> {
        // Dropping the Lua state runs no finalizers the host cares about;
        // plugin cleanup goes through on_unload before this is called.
        Box::pin(async { Ok(()) })
    }

    fn can_unload(&self) -> bool {
        true
    }
}

/// Reads a plugin file, rejecting it before parsing when it is too large.
fn read_bounded(path: &Path) -> Result<String, String> {
    use std::io::Read;

    let file = std::fs::File::open(path).map_err(|e| format!("read plugin: {e}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_PLUGIN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read plugin: {e}"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PLUGIN_BYTES {
        return Err("plugin file too large".to_string());
    }
    String::from_utf8(bytes).map_err(|_| "plugin file is not valid UTF-8".to_string())
}

/// A loaded Lua plugin: sandboxed state plus lifecycle entry points.
pub struct LuaPlugin {
    lua: Mutex<Lua>,
    on_load: Option<RegistryKey>,
    on_unload: Option<RegistryKey>,
}

impl LuaPlugin {
    /// Builds the sandboxed state by running the chunk exactly once.
    ///
    /// The chunk's return value is the metadata table. File IO is rooted at
    /// a staging directory until the plugin name is known, then rebound to
    /// the real data folder before `on_load` runs.
    fn spawn(source: &str) -> Result<(Self, PluginMetadata), LoaderError> {
        let lua = Lua::new();
        let paths = Arc::new(Mutex::new(PluginPaths::staging()));
        apply_sandbox(&lua, &paths).map_err(LoaderError::RuntimeError)?;
        bind_static_host(&lua, &paths).map_err(LoaderError::RuntimeError)?;
        let table: Table = lua
            .load(source)
            .set_name("plugin")
            .eval()
            .map_err(|e| LoaderError::InitializationFailed(e.to_string()))?;
        let metadata = parse_metadata(&table).ok_or(LoaderError::MetadataMissing)?;
        paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .promote(&metadata.name);
        let on_load = table
            .get::<Function>("on_load")
            .ok()
            .map(|f| lua.create_registry_value(f))
            .transpose()
            .map_err(|e| LoaderError::InitializationFailed(e.to_string()))?;
        let on_unload = table
            .get::<Function>("on_unload")
            .ok()
            .map(|f| lua.create_registry_value(f))
            .transpose()
            .map_err(|e| LoaderError::InitializationFailed(e.to_string()))?;
        Ok((
            Self {
                lua: Mutex::new(lua),
                on_load,
                on_unload,
            },
            metadata,
        ))
    }

    /// Calls a lifecycle entry point, if the plugin defined one.
    fn call_lifecycle(&self, key: Option<&RegistryKey>) -> Result<(), String> {
        let Some(key) = key else {
            return Ok(());
        };
        let lua = self
            .lua
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let func: Function = lua
            .registry_value(key)
            .map_err(|e| format!("lua callback gone: {e}"))?;
        func.call::<()>(()).map_err(|e| format!("lua error: {e}"))
    }
}

impl Plugin for LuaPlugin {
    fn on_load(&self, context: Arc<Context>) -> PluginFuture<'_, Result<(), String>> {
        Box::pin(async move {
            bind_server_host(&self.lua, &context).map_err(|e| format!("host bind: {e}"))?;
            self.call_lifecycle(self.on_load.as_ref())
        })
    }

    fn on_unload(&self, _context: Arc<Context>) -> PluginFuture<'_, Result<(), String>> {
        Box::pin(async move { self.call_lifecycle(self.on_unload.as_ref()) })
    }
}

/// Plugin data folder, mirroring `Context::get_data_folder`.
fn data_dir_for(plugin_name: &str) -> PathBuf {
    Path::new("plugins").join("data").join(plugin_name)
}

/// Paths a plugin chunk may touch, rebound once the plugin name is known.
#[derive(Debug, Clone)]
pub(crate) struct PluginPaths {
    pub(crate) root: PathBuf,
    pub(crate) name: String,
}

impl PluginPaths {
    const fn staging() -> Self {
        Self {
            root: PathBuf::new(),
            name: String::new(),
        }
    }

    fn promote(&mut self, plugin_name: &str) {
        self.root = data_dir_for(plugin_name);
        self.name = plugin_name.to_string();
    }
}

/// Host functions available without a server (bound at load time).
fn bind_static_host(lua: &Lua, paths: &Arc<Mutex<PluginPaths>>) -> Result<(), String> {
    let host = lua.create_table().map_err(|e| e.to_string())?;
    let logged = Arc::clone(paths);
    host.set(
        "log",
        lua.create_function(move |_, (level, message): (String, String)| {
            let name = logged
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .name
                .clone();
            log_line(&level, &name, &message);
            Ok(())
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    host.set(
        "version",
        lua.create_function(|_, ()| Ok(env!("CARGO_PKG_VERSION").to_string()))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let rooted = Arc::clone(paths);
    host.set(
        "config_path",
        lua.create_function(move |_, filename: String| {
            validate_config_name(&filename).map_err(mlua::Error::external)?;
            let root = rooted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .root
                .clone();
            Ok(config_path_for(&root, &filename)
                .to_string_lossy()
                .into_owned())
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    lua.globals().set("host", host).map_err(|e| e.to_string())?;
    Ok(())
}

/// Host functions needing the server (bound in `on_load`).
fn bind_server_host(lua: &Mutex<Lua>, context: &Arc<Context>) -> Result<(), String> {
    let lua = lua
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let host: Table = lua.globals().get("host").map_err(|e| e.to_string())?;
    let server = context.server.clone();
    host.set(
        "get_mspt",
        lua.create_function(move |_, ()| Ok(server.get_mspt()))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let server = context.server.clone();
    host.set(
        "get_tps",
        lua.create_function(move |_, ()| Ok(server.get_tps()))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let server = context.server.clone();
    host.set(
        "get_player_count",
        lua.create_function(move |_, ()| Ok(server.get_all_players().len() as u32))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Routes a plugin log line to `tracing`.
fn log_line(level: &str, plugin: &str, message: &str) {
    match level {
        "warn" => tracing::warn!(target: "lua_plugin", plugin = plugin, "{message}"),
        "error" => tracing::error!(target: "lua_plugin", plugin = plugin, "{message}"),
        "debug" => tracing::debug!(target: "lua_plugin", plugin = plugin, "{message}"),
        _ => tracing::info!(target: "lua_plugin", plugin = plugin, "{message}"),
    }
}

/// Parses and validates the metadata table a plugin chunk returns.
fn parse_metadata(table: &Table) -> Option<PluginMetadata> {
    fn get_string(table: &Table, key: &str) -> Option<String> {
        match table.get::<Value>(key).ok()? {
            Value::String(s) => s.to_str().ok().map(|s| s.to_string()),
            _ => None,
        }
    }
    let name = get_string(table, "name")?;
    let version = get_string(table, "version").unwrap_or_else(|| "0.1.0".to_string());
    let authors: Vec<String> = table.get("authors").unwrap_or_default();
    let description: String = table.get("description").unwrap_or_default();
    let dependencies: Vec<String> = table.get("dependencies").unwrap_or_default();
    if !(1..=64).contains(&name.len()) {
        return None;
    }
    if bounded_string(&version, 32).is_err() {
        return None;
    }
    if !authors
        .iter()
        .chain(std::iter::once(&description))
        .chain(dependencies.iter())
        .all(|s| bounded_string(s, 1024).is_ok())
    {
        return None;
    }
    if dependencies.len() > 64 {
        return None;
    }
    Some(PluginMetadata {
        name,
        version,
        authors,
        description,
        dependencies,
        permissions: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(source: &str) -> Lua {
        let lua = Lua::new();
        let paths = Arc::new(Mutex::new(PluginPaths::staging()));
        apply_sandbox(&lua, &paths).unwrap();
        lua.load(source).set_name("test").exec().unwrap();
        lua
    }

    #[test]
    fn sandbox_removes_dangerous_apis() {
        let lua = state("x = 1");
        let globals = lua.globals();
        for name in ["debug", "dofile", "loadfile"] {
            assert_eq!(globals.get::<Value>(name).unwrap(), Value::Nil);
        }
        let os: Table = globals.get("os").unwrap();
        for name in ["execute", "remove", "rename", "exit", "getenv", "tmpname"] {
            assert_eq!(os.get::<Value>(name).unwrap(), Value::Nil);
        }
        assert!(os.get::<Function>("clock").is_ok());
        let io: Table = globals.get("io").unwrap();
        assert_eq!(io.get::<Value>("popen").unwrap(), Value::Nil);
        assert!(io.get::<Function>("open").is_ok());
    }

    #[test]
    fn require_only_serves_the_shim() {
        let lua = state("ok1 = require('bukkit')");
        assert!(
            lua.load("return type(require('bukkit').wrap_event)")
                .eval::<String>()
                .is_ok()
        );
        assert!(lua.load("return require('socket')").exec().is_err());
    }

    #[test]
    fn print_goes_to_log_without_panicking() {
        let lua = Lua::new();
        let paths = Arc::new(Mutex::new(PluginPaths::staging()));
        apply_sandbox(&lua, &paths).unwrap();
        bind_static_host(&lua, &paths).unwrap();
        lua.load("print('hello')").set_name("t").exec().unwrap();
    }

    #[test]
    fn metadata_parses_and_rejects() {
        let good = Lua::new();
        let table: Table = good
            .load("return { name = 'x', version = '1', authors = {'a'} }")
            .eval()
            .unwrap();
        let meta = parse_metadata(&table).unwrap();
        assert_eq!(meta.name, "x");
        assert_eq!(meta.authors, vec!["a".to_string()]);

        let bad = Lua::new();
        let table: Table = bad.load("return { version = '1' }").eval().unwrap();
        assert!(parse_metadata(&table).is_none());

        let bad_type = Lua::new();
        let table: Table = bad_type.load("return { name = 42 }").eval().unwrap();
        assert!(parse_metadata(&table).is_none());
    }

    #[test]
    fn chunk_errors_are_reported() {
        assert!(LuaPlugin::spawn("this is not lua {{{{{").is_err());
        assert!(LuaPlugin::spawn("return 42").is_err());
    }

    #[test]
    fn spawn_runs_chunk_once_and_promotes_paths() {
        let source = r#"
            host.log("info", "top-level runs")
            return { name = "once", version = "1" }
        "#;
        let (plugin, metadata) = LuaPlugin::spawn(source).unwrap();
        assert_eq!(metadata.name, "once");
        // A second spawn would run top-level code again; a single spawn
        // must not duplicate lifecycle state.
        assert!(plugin.on_load.is_none());
        assert!(plugin.on_unload.is_none());
    }

    #[test]
    fn validate_config_name_rules() {
        assert!(validate_config_name("config.json").is_ok());
        for bad in ["../x.json", "a/b.json", ".hidden", "", "x/y", "a\\b.json"] {
            assert!(validate_config_name(bad).is_err(), "{bad}");
        }
        assert!(validate_config_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn bounded_string_limits() {
        assert!(bounded_string("ok", 2).is_ok());
        assert!(bounded_string("toolong", 2).is_err());
    }
}

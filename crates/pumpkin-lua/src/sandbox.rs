//! Sandbox policy for Lua plugin states.
//!
//! Everything a plugin chunk can reach is decided here. The shape mirrors
//! the `host` contract in `lua/bukkit.lua`: file access only through a
//! data-folder-rooted `io.open`, no process control, no bytecode loading
//! from disk, `require` serving the embedded shim only.

use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, Table, Value};

use crate::PluginPaths;

/// Applies the sandbox to a fresh state, rooting file IO at `paths`.
pub fn apply_sandbox(lua: &Lua, paths: &Arc<Mutex<PluginPaths>>) -> Result<(), String> {
    strip_globals(lua)?;
    strip_os(lua)?;
    strip_io(lua, paths)?;
    restrict_require(lua)?;
    override_print(lua)
}

/// Removes libraries and globals plugins must never reach.
fn strip_globals(lua: &Lua) -> Result<(), String> {
    let globals = lua.globals();
    for name in ["debug", "dofile", "loadfile"] {
        globals.set(name, Value::Nil).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Keeps date/time/clock/difftime; drops everything that touches the system.
fn strip_os(lua: &Lua) -> Result<(), String> {
    let os: Table = lua.globals().get("os").map_err(|e| e.to_string())?;
    for name in [
        "execute",
        "exit",
        "getenv",
        "remove",
        "rename",
        "setlocale",
        "tmpname",
    ] {
        os.set(name, Value::Nil).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Removes raw IO handles and roots `io.open` at the plugin data folder.
///
/// The root is read on every call so promoting the plugin paths after the
/// metadata is known also re-roots already-created states.
fn strip_io(lua: &Lua, paths: &Arc<Mutex<PluginPaths>>) -> Result<(), String> {
    let io: Table = lua.globals().get("io").map_err(|e| e.to_string())?;
    for name in ["popen", "tmpfile", "lines", "input", "output"] {
        io.set(name, Value::Nil).map_err(|e| e.to_string())?;
    }
    let original: Function = io.get("open").map_err(|e| e.to_string())?;
    let rooted = Arc::clone(paths);
    let open = lua
        .create_function(move |_lua, (name, mode): (String, Option<String>)| {
            let root = rooted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .root
                .clone();
            let path = crate::validate::resolve_io_path(&root, &name)?;
            let mode = mode.unwrap_or_else(|| "r".to_string());
            original.call::<Value>((path.to_string_lossy().as_ref(), mode))
        })
        .map_err(|e| e.to_string())?;
    io.set("open", open).map_err(|e| e.to_string())?;
    Ok(())
}

/// Serves only the embedded `bukkit` shim through `require`.
fn restrict_require(lua: &Lua) -> Result<(), String> {
    let package: Table = lua.globals().get("package").map_err(|e| e.to_string())?;
    package.set("path", "").map_err(|e| e.to_string())?;
    package.set("cpath", "").map_err(|e| e.to_string())?;
    let preload: Table = package.get("preload").map_err(|e| e.to_string())?;
    preload
        .set(
            "bukkit",
            lua.create_function(|lua, ()| {
                lua.load(crate::SHIM_SOURCE)
                    .set_name("bukkit")
                    .eval::<Table>()
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    let require = lua
        .create_function(|lua, name: String| {
            if name == "bukkit" {
                let preload: Table = lua
                    .globals()
                    .get::<Table>("package")
                    .and_then(|p| p.get("preload"))
                    .map_err(mlua::Error::external)?;
                let loader: Function = preload.get("bukkit").map_err(mlua::Error::external)?;
                loader.call::<Table>(())
            } else {
                Err(mlua::Error::external(format!(
                    "require of '{name}' is blocked; only 'bukkit' is available"
                )))
            }
        })
        .map_err(|e| e.to_string())?;
    lua.globals()
        .set("require", require)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Routes `print` to the plugin log instead of stdout.
fn override_print(lua: &Lua) -> Result<(), String> {
    let print = lua
        .create_function(|_, args: mlua::Variadic<Value>| {
            let line = args
                .iter()
                .map(|value| format!("{value:?}"))
                .collect::<Vec<_>>()
                .join("\t");
            tracing::info!(target: "lua_plugin", "{line}");
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    lua.globals()
        .set("print", print)
        .map_err(|e| e.to_string())?;
    Ok(())
}

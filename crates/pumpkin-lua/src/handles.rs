//! Player/world handles and command execution for Lua plugins.
//!
//! Handles are opaque `UserData` values: Lua code passes them back to `host`
//! functions, exactly like the `bukkit.lua` shim expects. Commands registered
//! from Lua run a Rust closure that re-enters the plugin's `Lua` state; all
//! access stays behind the plugin `Mutex`, never concurrent.

use std::sync::Arc;

use parking_lot::ReentrantMutex;

use mlua::{AnyUserData, Function, Lua, RegistryKey, Table, Value};
use pumpkin_core::{
    command::{
        argument_builder::{self, ArgumentBuilder},
        context::{command_context::CommandContext, command_source::CommandSource},
        node::CommandExecutor,
    },
    entity::player::Player,
    plugin::api::Context,
    server::Server,
    world::World,
};
use pumpkin_util::{
    math::{vector2::Vector2, vector3::Vector3},
    text::TextComponent,
};

/// A player handle owned by Lua code.
pub struct LuaPlayer {
    player: Arc<Player>,
    server: Arc<Server>,
}

impl mlua::UserData for LuaPlayer {}

/// A world handle owned by Lua code.
pub struct LuaWorld {
    world: Arc<World>,
}

impl mlua::UserData for LuaWorld {}

/// Extracts the player behind a Lua handle value.
fn lua_player(value: Value) -> Result<Arc<Player>, mlua::Error> {
    match value {
        Value::UserData(data) => data
            .borrow::<LuaPlayer>()
            .map(|guard| guard.player.clone())
            .map_err(|e| mlua::Error::external(e.to_string())),
        _ => Err(mlua::Error::external("expected a player handle")),
    }
}

/// Extracts the server stored alongside a player handle.
fn lua_player_server(value: &Value) -> Result<Arc<Server>, mlua::Error> {
    match value {
        Value::UserData(data) => data
            .borrow::<LuaPlayer>()
            .map(|guard| guard.server.clone())
            .map_err(|e| mlua::Error::external(e.to_string())),
        _ => Err(mlua::Error::external("expected a player handle")),
    }
}

/// Extracts the world behind a Lua handle value.
fn lua_world(value: Value) -> Result<Arc<World>, mlua::Error> {
    match value {
        Value::UserData(data) => data
            .borrow::<LuaWorld>()
            .map(|guard| guard.world.clone())
            .map_err(|e| mlua::Error::external(e.to_string())),
        _ => Err(mlua::Error::external("expected a world handle")),
    }
}

fn push_player(
    lua: &Lua,
    server: &Arc<Server>,
    player: &Arc<Player>,
) -> Result<AnyUserData, String> {
    lua.create_userdata(LuaPlayer {
        player: Arc::clone(player),
        server: Arc::clone(server),
    })
    .map_err(|e| e.to_string())
}

/// Binds the server-backed `host` functions (players, worlds, commands).
pub fn bind_game_host(
    lua: &Lua,
    context: &Arc<Context>,
    plugin_lua: &Arc<ReentrantMutex<Lua>>,
    plugin_name: &str,
) -> Result<(), String> {
    let host: Table = lua.globals().get("host").map_err(|e| e.to_string())?;
    bind_player_lookup(lua, &host, &context.server)?;
    bind_player_actions(lua, &host, &context.server)?;
    bind_dispatch(lua, &host, &context.server)?;
    bind_register_command(lua, &host, context, plugin_lua, plugin_name)?;
    Ok(())
}

/// Binds player lookup: `get_player_by_name`, `get_all_players`.
fn bind_player_lookup(lua: &Lua, host: &Table, server: &Arc<Server>) -> Result<(), String> {
    let every = Arc::clone(server);
    host.set(
        "get_player_by_name",
        lua.create_function(move |lua, name: String| {
            every
                .get_player_by_name(&name)
                .map(|player| push_player(lua, &every, &player))
                .transpose()
                .map_err(mlua::Error::external)
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    let every = Arc::clone(server);
    host.set(
        "get_all_players",
        lua.create_function(move |lua, ()| {
            every
                .get_all_players()
                .iter()
                .map(|player| push_player(lua, &every, player))
                .collect::<Result<Vec<_>, _>>()
                .map_err(mlua::Error::external)
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Binds per-player actions: name, message, kick, teleport, permission,
/// world and location.
fn bind_player_actions(lua: &Lua, host: &Table, server: &Arc<Server>) -> Result<(), String> {
    host.set(
        "player_get_name",
        lua.create_function(|_, player: Value| {
            lua_player(player).map(|p| p.gameprofile.name.clone())
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    host.set(
        "player_send_message",
        lua.create_function(|_, (player, text): (Value, String)| {
            let target = lua_player(player)?;
            target.send_message(
                &TextComponent::text(text),
                0,
                &TextComponent::text("Server"),
                None,
            );
            Ok(())
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    host.set(
        "player_kick",
        lua.create_function(|_, (player, reason): (Value, String)| {
            let target = lua_player(player)?;
            target.kick(
                pumpkin_core::net::DisconnectReason::Kicked,
                &TextComponent::text(reason),
            );
            Ok(())
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    bind_player_movement(lua, host, server)?;
    Ok(())
}

/// Binds movement queries: teleport, world and location.
fn bind_player_movement(lua: &Lua, host: &Table, server: &Arc<Server>) -> Result<(), String> {
    let spawner = Arc::clone(server);
    host.set(
        "player_teleport",
        lua.create_function(
            move |_,
                  (player, world, x, y, z, yaw, pitch): (
                Value,
                Value,
                f64,
                f64,
                f64,
                Option<f32>,
                Option<f32>,
            )| {
                let target = lua_player(player)?;
                let destination = lua_world(world)?;
                let position = Vector3::new(x, y, z);
                spawner.spawn_task(async move {
                    target
                        .teleport_world(destination, position, yaw, pitch)
                        .await;
                });
                Ok(())
            },
        )
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    host.set(
        "player_has_permission",
        lua.create_function(|_, (player, node): (Value, String)| {
            let target = lua_player(player.clone())?;
            let permission_server = lua_player_server(&player)?;
            Ok(target.has_permission(&permission_server, &node))
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    host.set(
        "player_get_world",
        lua.create_function(|lua, player: Value| {
            let target = lua_player(player)?;
            let world = target.living_entity.entity.world.load_full();
            lua.create_userdata(LuaWorld { world })
                .map_err(mlua::Error::external)
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    host.set(
        "player_get_location",
        lua.create_function(|lua, player: Value| {
            let target = lua_player(player)?;
            let entity = &target.living_entity.entity;
            let pos = entity.pos.load();
            let table = lua.create_table().map_err(mlua::Error::external)?;
            table.set("x", pos.x).map_err(mlua::Error::external)?;
            table.set("y", pos.y).map_err(mlua::Error::external)?;
            table.set("z", pos.z).map_err(mlua::Error::external)?;
            table
                .set("yaw", entity.yaw.load())
                .map_err(mlua::Error::external)?;
            table
                .set("pitch", entity.pitch.load())
                .map_err(mlua::Error::external)?;
            Ok(table)
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Binds programmatic dispatch: `execute_command(command, sender)`.
fn bind_dispatch(lua: &Lua, host: &Table, server: &Arc<Server>) -> Result<(), String> {
    let dispatcher = Arc::clone(server);
    host.set(
        "execute_command",
        lua.create_function(move |_, (command, sender): (String, String)| {
            let source = command_source(&dispatcher, &sender)?;
            dispatcher
                .command_dispatcher
                .load()
                .handle_command(&source, command.trim_start_matches('/'));
            Ok(true)
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Builds a command source for programmatic dispatch.
///
/// `"console"` runs as the server; `"player:<name>"` runs as that player.
fn command_source(server: &Arc<Server>, sender: &str) -> Result<CommandSource, mlua::Error> {
    use pumpkin_core::command::CommandSender;

    if let Some(name) = sender.strip_prefix("player:") {
        let player = server
            .get_player_by_name(name)
            .ok_or_else(|| mlua::Error::external(format!("no such player: {name}")))?;
        return Ok(player.get_command_source(server));
    }
    if sender == "console" {
        let world = server
            .worlds
            .load()
            .first()
            .cloned()
            .ok_or_else(|| mlua::Error::external("no world loaded"))?;
        return Ok(CommandSource::new(
            CommandSender::Console,
            world,
            None,
            Vector3::default(),
            Vector2::default(),
            "Server".to_string(),
            TextComponent::text("Server"),
            Arc::clone(server),
        ));
    }
    Err(mlua::Error::external(format!("unknown sender: {sender}")))
}

/// A command executor that re-enters Lua.
///
/// Built once per `host.register_command`; every invocation locks the plugin
/// state, so Lua callbacks never run concurrently.
struct LuaCommandExecutor {
    lua: Arc<ReentrantMutex<Lua>>,
    key: RegistryKey,
    owner: String,
}

impl CommandExecutor for LuaCommandExecutor {
    fn execute(&self, context: &CommandContext) -> pumpkin_command::node::CommandExecutorResult {
        let sender = context.source.name.clone();
        let is_player = context.source.entity.is_some();
        let input = context.input.clone();
        let lua = self.lua.lock();
        let callback: Function = match lua.registry_value(&self.key) {
            Ok(callback) => callback,
            Err(e) => {
                report_command_failure(&context.source, &e.to_string());
                return Ok(0);
            }
        };
        match callback.call::<bool>((sender, is_player, input)) {
            Ok(_) => Ok(1),
            Err(e) => {
                tracing::error!(target: "lua_plugin", plugin = %self.owner, "command failed: {e}");
                report_command_failure(&context.source, &e.to_string());
                Ok(0)
            }
        }
    }
}
fn bind_register_command(
    lua: &Lua,
    host: &Table,
    context: &Arc<Context>,
    plugin_lua: &Arc<ReentrantMutex<Lua>>,
    plugin_name: &str,
) -> Result<(), String> {
    let registrar = Arc::clone(context);
    let state = Arc::clone(plugin_lua);
    let owner = plugin_name.to_string();
    host.set(
        "register_command",
        lua
            .create_function(
                move |_,
                      (name, description, permission, callback): (
                    String,
                    String,
                    String,
                    Function,
                )| {
                    let key = state
                        .lock()
                        .create_registry_value(callback)
                        .map_err(mlua::Error::external)?;
                    let executor = LuaCommandExecutor {
                        lua: Arc::clone(&state),
                        key,
                        owner: owner.clone(),
                    };
                    let node = argument_builder::command(name, description).executes(executor);
                    registrar.register_command(node, permission);
                    Ok(())
                },
            )
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Reports a failed Lua command back to its sender.
fn report_command_failure(source: &CommandSource, message: &str) {
    source.send_error(TextComponent::text(format!("Lua error: {message}")));
}

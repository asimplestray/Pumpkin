//! Event bridging for Lua plugins.
//!
//! `host.register_listener(name, fn)` maps a Bukkit-style event name to a
//! native [`Payload`] registration. The Lua callback receives a table with
//! the event data (`player` handle, message fields, `cancelled`) and mutates
//! it in place; mutations are written back before the handler returns, so
//! this always registers blocking handlers.
//!
//! Only events with a [`LuaEventBridge`] impl are exposed. Adding one is
//! mechanical: implement the two conversions and add a match arm in
//! [`bind_register_listener`]. Priority is always `Normal` for now.

use std::sync::Arc;

use mlua::{Function, Lua, Table};
use parking_lot::ReentrantMutex;
use pumpkin_core::{
    plugin::{
        EventHandler, EventPriority, Payload,
        api::{
            Context,
            events::player::{
                player_chat::PlayerChatEvent, player_join::PlayerJoinEvent,
                player_leave::PlayerLeaveEvent,
            },
        },
    },
    server::Server,
};
use pumpkin_util::text::TextComponent;

use crate::handles::push_player;

/// Converts one native event to a Lua table and writes mutations back.
pub trait LuaEventBridge: Payload {
    /// Builds the Lua-visible table for this event.
    fn push_to_lua(lua: &Lua, server: &Arc<Server>, event: &Self) -> Result<Table, mlua::Error>;

    /// Writes Lua-side mutations back into the event.
    fn pull_from_lua(event: &mut Self, table: Table) -> Result<(), mlua::Error>;
}

/// A Lua callback registered as a native event handler.
struct LuaEventListener<E: LuaEventBridge> {
    lua: Arc<ReentrantMutex<Lua>>,
    key: mlua::RegistryKey,
    server: Arc<Server>,
    event: std::marker::PhantomData<E>,
}

impl<E: LuaEventBridge> EventHandler<E> for LuaEventListener<E> {
    fn handle_blocking<'a>(
        &'a self,
        _server: &'a Arc<Server>,
        event: &'a mut E,
    ) -> pumpkin_core::plugin::BoxFuture<'a, ()> {
        Box::pin(async move {
            let lua = self.lua.lock();
            let table = match E::push_to_lua(&lua, &self.server, event) {
                Ok(table) => table,
                Err(e) => {
                    tracing::error!(target: "lua_plugin", "event bridge failed: {e}");
                    return;
                }
            };
            let callback: Function = match lua.registry_value(&self.key) {
                Ok(callback) => callback,
                Err(e) => {
                    tracing::error!(target: "lua_plugin", "event callback gone: {e}");
                    return;
                }
            };
            if let Err(e) = callback.call::<()>(table.clone()) {
                tracing::error!(target: "lua_plugin", "event callback failed: {e}");
                return;
            }
            if let Err(e) = E::pull_from_lua(event, table) {
                tracing::error!(target: "lua_plugin", "event writeback failed: {e}");
            }
        })
    }
}

fn player_table(
    lua: &Lua,
    server: &Arc<Server>,
    player: &std::sync::Arc<pumpkin_core::entity::player::Player>,
) -> Result<mlua::AnyUserData, mlua::Error> {
    push_player(lua, server, player).map_err(mlua::Error::external)
}

impl LuaEventBridge for PlayerJoinEvent {
    fn push_to_lua(lua: &Lua, server: &Arc<Server>, event: &Self) -> Result<Table, mlua::Error> {
        let table = lua.create_table()?;
        table.set("player", player_table(lua, server, &event.player)?)?;
        table.set(
            "join_message",
            event.join_message.clone().to_pretty_console(),
        )?;
        table.set("cancelled", event.cancelled)?;
        Ok(table)
    }

    fn pull_from_lua(event: &mut Self, table: Table) -> Result<(), mlua::Error> {
        if let Ok(message) = table.get::<String>("join_message") {
            event.join_message = TextComponent::text(message);
        }
        if let Ok(cancelled) = table.get::<bool>("cancelled") {
            event.cancelled = cancelled;
        }
        Ok(())
    }
}

impl LuaEventBridge for PlayerLeaveEvent {
    fn push_to_lua(lua: &Lua, server: &Arc<Server>, event: &Self) -> Result<Table, mlua::Error> {
        let table = lua.create_table()?;
        table.set("player", player_table(lua, server, &event.player)?)?;
        table.set(
            "leave_message",
            event.leave_message.clone().to_pretty_console(),
        )?;
        table.set("cancelled", event.cancelled)?;
        Ok(table)
    }

    fn pull_from_lua(event: &mut Self, table: Table) -> Result<(), mlua::Error> {
        if let Ok(message) = table.get::<String>("leave_message") {
            event.leave_message = TextComponent::text(message);
        }
        if let Ok(cancelled) = table.get::<bool>("cancelled") {
            event.cancelled = cancelled;
        }
        Ok(())
    }
}

impl LuaEventBridge for PlayerChatEvent {
    fn push_to_lua(lua: &Lua, server: &Arc<Server>, event: &Self) -> Result<Table, mlua::Error> {
        let table = lua.create_table()?;
        table.set("player", player_table(lua, server, &event.player)?)?;
        table.set("message", event.message.clone())?;
        table.set("cancelled", event.cancelled)?;
        Ok(table)
    }

    fn pull_from_lua(event: &mut Self, table: Table) -> Result<(), mlua::Error> {
        if let Ok(message) = table.get::<String>("message") {
            event.message = message;
        }
        if let Ok(cancelled) = table.get::<bool>("cancelled") {
            event.cancelled = cancelled;
        }
        Ok(())
    }
}

/// Exposes `host.register_listener(event_name, fn)`.
pub fn bind_register_listener(
    lua: &Lua,
    host: &Table,
    context: &Arc<Context>,
    plugin_lua: &Arc<ReentrantMutex<Lua>>,
) -> Result<(), String> {
    let registrar = Arc::clone(context);
    let state = Arc::clone(plugin_lua);
    let server = context.server.clone();
    host.set(
        "register_listener",
        lua.create_function(move |_, (name, callback): (String, Function)| {
            let key = state
                .lock()
                .create_registry_value(callback)
                .map_err(mlua::Error::external)?;
            match name.as_str() {
                "PlayerJoinEvent" => {
                    registrar.register_event::<PlayerJoinEvent, _>(
                        Arc::new(LuaEventListener {
                            lua: Arc::clone(&state),
                            key,
                            server: Arc::clone(&server),
                            event: std::marker::PhantomData,
                        }),
                        EventPriority::Normal,
                        true,
                    );
                }
                "PlayerLeaveEvent" | "PlayerQuitEvent" => {
                    registrar.register_event::<PlayerLeaveEvent, _>(
                        Arc::new(LuaEventListener {
                            lua: Arc::clone(&state),
                            key,
                            server: Arc::clone(&server),
                            event: std::marker::PhantomData,
                        }),
                        EventPriority::Normal,
                        true,
                    );
                }
                "PlayerChatEvent" | "AsyncPlayerChatEvent" => {
                    registrar.register_event::<PlayerChatEvent, _>(
                        Arc::new(LuaEventListener {
                            lua: Arc::clone(&state),
                            key,
                            server: Arc::clone(&server),
                            event: std::marker::PhantomData,
                        }),
                        EventPriority::Normal,
                        true,
                    );
                }
                _ => {
                    return Err(mlua::Error::external(format!("unknown event: {name}")));
                }
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

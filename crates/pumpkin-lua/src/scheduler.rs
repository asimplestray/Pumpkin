//! Tick-based task scheduler for Lua plugins.
//!
//! The loader owns no plugin instances, so scheduled callbacks live in one
//! process-wide list keyed by task id. This matches Pumpkin's process model
//! of a single server: entries hold their plugin's `Lua` state, and
//! [`tick_tasks`] runs them from [`PluginLoader::on_tick`]. Locks are never
//! held across a Lua call.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use mlua::{Function, Lua, RegistryKey};
use parking_lot::ReentrantMutex;

struct LuaTask {
    id: u64,
    lua: Arc<ReentrantMutex<Lua>>,
    key: RegistryKey,
    remaining: u64,
    period: Option<u64>,
}

static TASKS: parking_lot::Mutex<Vec<LuaTask>> = parking_lot::Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Schedules a Lua callback, once or repeating. Returns the task id.
pub fn schedule(
    lua: &Arc<ReentrantMutex<Lua>>,
    delay_ticks: u64,
    period_ticks: Option<u64>,
    callback: Function,
) -> Result<u64, String> {
    let key = lua
        .lock()
        .create_registry_value(callback)
        .map_err(|e| e.to_string())?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    TASKS.lock().push(LuaTask {
        id,
        lua: Arc::clone(lua),
        key,
        remaining: delay_ticks,
        period: period_ticks,
    });
    Ok(id)
}

/// Cancels a scheduled task. Unknown ids are ignored.
pub fn cancel(id: u64) {
    TASKS.lock().retain(|task| task.id != id);
}

/// Runs due tasks. Called once per server tick.
pub fn tick_tasks() {
    let due: Vec<LuaTask> = {
        let mut tasks = TASKS.lock();
        let mut due = Vec::new();
        let mut kept = Vec::new();
        for task in tasks.drain(..) {
            let mut task = task;
            if task.remaining > 0 {
                task.remaining -= 1;
            }
            if task.remaining == 0 {
                due.push(task);
            } else {
                kept.push(task);
            }
        }
        *tasks = kept;
        due
    };
    let mut repeating = Vec::new();
    for mut task in due {
        let callback: Result<Function, _> = task.lua.lock().registry_value(&task.key);
        match callback {
            Ok(callback) => {
                if let Err(e) = callback.call::<()>(()) {
                    tracing::error!(target: "lua_plugin", task = task.id, "scheduled task failed: {e}");
                }
            }
            Err(e) => {
                tracing::error!(target: "lua_plugin", task = task.id, "scheduled task gone: {e}");
            }
        }
        if let Some(period) = task.period {
            task.remaining = period;
            repeating.push(task);
        }
    }
    if !repeating.is_empty() {
        TASKS.lock().extend(repeating);
    }
}

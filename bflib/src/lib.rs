use dcso3::{coalition::{Side, Coalition}, event::Event, world::World, String, UserHooks, group::Group, Vec2, country::Country, env};
use mlua::prelude::*;

enum SpawnLoc {
    AtPos(Vec2),
    AtTrigger(String),
}

/* 
fn spawn(lua: &Lua, side: Side, location: SpawnLoc, name: &str) -> LuaResult<Group> {
    let coalition = Coalition::singleton(lua)?;
    let miz = env::miz::Miz::singleton(lua)?;
    let mizcoa = miz.coalition(side)?;
    for country in mizcoa.countries() {
        
    }
}
*/

fn on_player_try_connect(
    _: &Lua,
    addr: String,
    name: String,
    ucid: String,
    id: u32,
) -> LuaResult<bool> {
    println!(
        "onPlayerTryConnect addr: {:?}, name: {:?}, ucid: {:?}, id: {:?}",
        addr, name, ucid, id
    );
    Ok(true)
}

fn on_player_try_send_chat(_: &Lua, id: u32, msg: String, all: bool) -> LuaResult<String> {
    println!(
        "onPlayerTrySendChat id: {:?}, msg: {:?}, all: {:?}",
        id, msg, all
    );
    Ok(msg)
}

fn on_player_try_change_slot(_: &Lua, id: u32, side: Side, slot: String) -> LuaResult<bool> {
    println!(
        "onPlayerTryChangeSlot id: {:?}, side: {:?}, slot: {:?}",
        id, side, slot
    );
    Ok(true)
}

fn on_event(_lua: &Lua, ev: Event) -> LuaResult<()> {
    println!("onEventTranslated: {:#?}", ev);
    Ok(())
}

fn init_hooks(lua: &Lua, _: ()) -> LuaResult<()> {
    UserHooks::new(lua)
        .on_player_try_change_slot(on_player_try_change_slot)?
        .on_player_try_connect(on_player_try_connect)?
        .on_player_try_send_chat(on_player_try_send_chat)?
        .register()
}

fn init_miz(lua: &Lua, _: ()) -> LuaResult<()> {
    World::get(lua)?.add_event_handler(on_event)?;
    Ok(())
}

#[mlua::lua_module]
fn bflib(lua: &Lua) -> LuaResult<LuaTable> {
    let exports = lua.create_table()?;
    exports.set("initHooks", lua.create_function(init_hooks)?)?;
    exports.set("initMiz", lua.create_function(init_miz)?)?;
    Ok(exports)
}

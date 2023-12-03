pub mod cfg;
pub mod db;
extern crate nalgebra as na;
use chrono::{prelude::*, Duration};
use compact_str::format_compact;
use db::{Db, UnitId};
use dcso3::{
    coalition::Side,
    env::{self, miz::Miz, Env},
    err,
    event::Event,
    lfs::Lfs,
    net::{Net, PlayerId, SlotId, Ucid},
    timer::Timer,
    unit::Unit,
    world::World,
    HooksLua, LuaEnv, MizLua, String, UserHooks, Vector2,
};
use fxhash::{FxHashMap, FxHashSet};
use mlua::prelude::*;
use std::{path::PathBuf, sync::mpsc, thread};

use crate::{cfg::Cfg, db::SlotAuth};

#[derive(Debug)]
enum BgTask {
    MizInit,
    SaveState(PathBuf, Db),
}

fn background_loop(rx: mpsc::Receiver<BgTask>) {
    while let Ok(msg) = rx.recv() {
        match msg {
            BgTask::MizInit => (),
            BgTask::SaveState(path, db) => match db.save(&path) {
                Ok(()) => (),
                Err(e) => println!("failed to save state to {:?}, {:?}", path, e),
            },
        }
    }
}

#[derive(Debug)]
struct PlayerInfo {
    name: String,
    ucid: Ucid,
}

#[derive(Debug, Default)]
struct Context {
    idx: env::miz::MizIndex,
    db: Db,
    to_background: Option<mpsc::Sender<BgTask>>,
    units_by_obj_id: FxHashMap<i64, UnitId>,
    info_by_player_id: FxHashMap<PlayerId, PlayerInfo>,
    id_by_ucid: FxHashMap<Ucid, PlayerId>,
    recently_landed: FxHashMap<SlotId, (String, DateTime<Utc>)>,
    force_to_spectators: FxHashSet<PlayerId>,
}

static mut CONTEXT: Option<Context> = None;

impl Context {
    // this must be used cautiously. Reasons why it's not totally nuts,
    // - the dcs scripting api is single threaded
    // - the event handlers can be triggerred by api calls, making refcells and mutexes error prone
    // - as long as an event handler doesn't step on state in an api call it's ok, since concurrency never happens
    //   that isn't so hard to guarantee
    unsafe fn get_mut() -> &'static mut Context {
        match CONTEXT.as_mut() {
            Some(ctx) => ctx,
            None => {
                println!("init ctx");
                CONTEXT = Some(Context::default());
                CONTEXT.as_mut().unwrap()
            }
        }
    }

    unsafe fn _get() -> &'static Context {
        Context::get_mut()
    }

    fn do_background_task(&mut self, task: BgTask) {
        if self.to_background.is_none() {
            let (tx, rx) = mpsc::channel();
            self.to_background = Some(tx);
            thread::spawn(move || background_loop(rx));
        }
        match self.to_background.as_ref().unwrap().send(task) {
            Ok(()) => (),
            Err(_) => println!("background loop died"),
        }
    }

    fn respawn_groups(&mut self, lua: MizLua) -> LuaResult<()> {
        let spctx = db::SpawnCtx::new(lua)?;
        for (_, group) in self.db.groups() {
            self.db.respawn_group(&self.idx, &spctx, group)?
        }
        Ok(())
    }
}

fn get_player_info<'a, 'lua, L: LuaEnv<'lua>>(
    tbl: &'a mut FxHashMap<PlayerId, PlayerInfo>,
    rtbl: &'a mut FxHashMap<Ucid, PlayerId>,
    lua: L,
    id: PlayerId,
) -> LuaResult<&'a PlayerInfo> {
    if tbl.contains_key(&id) {
        Ok(&tbl[&id])
    } else {
        let net = Net::singleton(lua)?;
        let ifo = net.get_player_info(id)?;
        let ucid = ifo.ucid()?.ok_or_else(|| err("player has no ucid"))?;
        let name = ifo.name()?;
        rtbl.insert(ucid.clone(), id);
        tbl.insert(id, PlayerInfo { name, ucid });
        Ok(&tbl[&id])
    }
}

fn on_player_try_connect(
    _: HooksLua,
    addr: String,
    name: String,
    ucid: Ucid,
    id: PlayerId,
) -> LuaResult<bool> {
    println!(
        "onPlayerTryConnect addr: {:?}, name: {:?}, ucid: {:?}, id: {:?}",
        addr, name, ucid, id
    );
    let ctx = unsafe { Context::get_mut() };
    ctx.id_by_ucid.insert(ucid.clone(), id);
    ctx.info_by_player_id.insert(id, PlayerInfo { name, ucid });
    Ok(true)
}

fn register_player(lua: HooksLua, id: PlayerId, msg: String) -> LuaResult<String> {
    let net = Net::singleton(lua)?;
    let ctx = unsafe { Context::get_mut() };
    let ifo = get_player_info(&mut ctx.info_by_player_id, &mut ctx.id_by_ucid, lua, id)?;
    let side = if msg.eq_ignore_ascii_case("blue") {
        Side::Blue
    } else if msg.eq_ignore_ascii_case("red") {
        Side::Red
    } else {
        return Err(err("side is not blue or red"));
    };
    match ctx
        .db
        .register_player(ifo.ucid.clone(), ifo.name.clone(), side)
    {
        Ok(()) => {
            let msg = String::from(format_compact!("Welcome to the {:?} team. You may only occupy slots belonging to your team. Good luck!", side));
            net.send_chat_to(msg, id, None)?;
            net.send_chat(
                String::from(format_compact!("{} has joined {:?} team", ifo.name, side)),
                true,
            )?
        }
        Err((side_switches, orig_side)) => {
            let msg = String::from(match side_switches {
                None => format_compact!("You are already on the {:?} team. You may switch sides by typing -switch {:?}.", orig_side, side),
                Some(0) => format_compact!("You are already on {:?} team, and you may not switch sides.", orig_side),
                Some(1) => format_compact!("You are already on {:?} team. You may sitch sides 1 time by typing -switch {:?}.", orig_side, side),
                Some(n) => format_compact!("You are already on {:?} team. You may switch sides {n} times. Type -switch {:?}.", orig_side, side),
            });
            net.send_chat_to(msg, id, None)?
        }
    }
    Ok(String::from(""))
}

fn sideswitch_player(lua: HooksLua, id: PlayerId, msg: String) -> LuaResult<String> {
    let net = Net::singleton(lua)?;
    let ctx = unsafe { Context::get_mut() };
    let ifo = get_player_info(&mut ctx.info_by_player_id, &mut ctx.id_by_ucid, lua, id)?;
    let side = if msg.eq_ignore_ascii_case("-switch blue") {
        Side::Blue
    } else if msg.eq_ignore_ascii_case("-switch red") {
        Side::Red
    } else {
        return Err(err("side must be blue or red"));
    };
    match ctx.db.sideswitch_player(&ifo.ucid, side) {
        Ok(()) => {
            let msg = String::from(format_compact!("{} has switched to {:?}", ifo.name, side));
            net.send_chat(msg, true)?
        }
        Err(e) => net.send_chat_to(String::from(e), id, None)?,
    }
    Ok(String::from(""))
}

fn on_player_try_send_chat(
    lua: HooksLua,
    id: PlayerId,
    msg: String,
    all: bool,
) -> LuaResult<String> {
    println!(
        "onPlayerTrySendChat id: {:?}, msg: {:?}, all: {:?}",
        id, msg, all
    );
    if msg.eq_ignore_ascii_case("blue") || msg.eq_ignore_ascii_case("red") {
        register_player(lua, id, msg)
    } else if msg.eq_ignore_ascii_case("-switch blue") || msg.eq_ignore_ascii_case("-switch red") {
        sideswitch_player(lua, id, msg)
    } else {
        Ok(msg)
    }
}

fn try_occupy_slot(lua: HooksLua, net: &Net, id: PlayerId) -> LuaResult<bool> {
    let now = Utc::now();
    let ctx = unsafe { Context::get_mut() };
    let (side, slot) = net.get_slot(id)?;
    let ifo = get_player_info(&mut ctx.info_by_player_id, &mut ctx.id_by_ucid, lua, id)?;
    match ctx.db.try_occupy_slot(now, side, slot, &ifo.ucid) {
        SlotAuth::NoLives => {
            println!("player {}{:?} has no lives", ifo.name, ifo.ucid);
            Ok(false)
        }
        SlotAuth::NotRegistered(side) => {
            println!("player {}{:?} isn't registered", ifo.name, ifo.ucid);
            let msg = String::from(format_compact!(
                "You must join {:?} to use this slot. Type {:?} in chat.",
                side,
                side
            ));
            Net::singleton(lua)?.send_chat_to(msg, id, None)?;
            Ok(false)
        }
        SlotAuth::ObjectiveNotOwned => {
            println!(
                "player {}{:?} coalition does not own the objective",
                ifo.name, ifo.ucid
            );
            let msg = String::from(format_compact!(
                "{:?} does not own the objective associated with this slot",
                side
            ));
            Net::singleton(lua)?.send_chat_to(msg, id, None)?;
            Ok(false)
        }
        SlotAuth::Yes => Ok(true),
    }
}

fn on_player_change_slot(lua: HooksLua, id: PlayerId) -> LuaResult<()> {
    let net = Net::singleton(lua)?;
    match try_occupy_slot(lua, &net, id) {
        Err(e) => {
            println!("error checking slot {:?}", e);
            net.force_player_slot(id, Side::Neutral, SlotId::spectator())?
        }
        Ok(false) => net.force_player_slot(id, Side::Neutral, SlotId::spectator())?,
        Ok(true) => (),
    }
    Ok(())
}

fn force_player_in_slot_to_spectators(ctx: &mut Context, slot: &SlotId) {
    if let Some(ucid) = ctx.db.player_in_slot(slot) {
        if let Some(id) = ctx.id_by_ucid.get(ucid) {
            ctx.force_to_spectators.insert(*id);
        }
    }
}

fn on_event(_lua: MizLua, ev: Event) -> LuaResult<()> {
    println!("onEventTranslated: {:?}", ev);
    let ctx = unsafe { Context::get_mut() };
    match ev {
        Event::Birth(b) => {
            if let Ok(unit) = b.initiator.as_unit() {
                let name = unit.as_object()?.get_name()?;
                if let Some(su) = ctx.db.get_unit_by_name(name.as_str()) {
                    let uid = su.id;
                    let oid: i64 = unit.get_object_id()?;
                    ctx.units_by_obj_id.insert(oid, uid);
                }
            }
        }
        Event::Dead(e) | Event::UnitLost(e) | Event::PilotDead(e) => {
            if let Ok(unit) = e.initiator.as_unit() {
                let id = unit.get_object_id()?;
                if let Some(uid) = ctx.units_by_obj_id.remove(&id) {
                    ctx.db.unit_dead(uid, true, Utc::now());
                }
                let slot = SlotId::from(unit.get_id()?);
                ctx.recently_landed.remove(&slot);
                force_player_in_slot_to_spectators(ctx, &slot)
            }
        }
        Event::Ejection(e) => {
            if let Ok(unit) = e.initiator.as_unit() {
                let slot = SlotId::from(unit.get_id()?);
                ctx.recently_landed.remove(&slot);
                force_player_in_slot_to_spectators(ctx, &slot)
            }
        }
        Event::Takeoff(e) => {
            if let Ok(unit) = e.initiator.as_unit() {
                let slot = SlotId::from(unit.get_id()?);
                let ctx = unsafe { Context::get_mut() };
                ctx.db.takeoff(Utc::now(), slot.clone());
                ctx.recently_landed.remove(&slot);
            }
        }
        Event::Land(e) => {
            if let Ok(unit) = e.initiator.as_unit() {
                let slot = SlotId::from(unit.get_id()?);
                let name = unit.as_object()?.get_name()?;
                let ctx = unsafe { Context::get_mut() };
                ctx.recently_landed.insert(slot, (name, Utc::now()));
            }
        }
        _ => (),
    }
    Ok(())
}

fn on_mission_load_end(lua: HooksLua) -> LuaResult<()> {
    println!("on_mission_load_end");
    let miz = env::miz::Miz::singleton(lua)?;
    println!("indexing mission");
    let ctx = unsafe { Context::get_mut() };
    ctx.idx = miz.index()?;
    ctx.do_background_task(BgTask::MizInit);
    println!("indexed mission");
    Ok(())
}

fn on_simulation_start(_lua: HooksLua) -> LuaResult<()> {
    println!("on_simulation_start");
    Ok(())
}

fn init_hooks(lua: HooksLua) -> LuaResult<()> {
    println!("setting user hooks");
    UserHooks::new(lua)
        .on_simulation_start(on_simulation_start)?
        .on_mission_load_end(on_mission_load_end)?
        .on_player_change_slot(on_player_change_slot)?
        .on_player_try_connect(on_player_try_connect)?
        .on_player_try_send_chat(on_player_try_send_chat)?
        .register()?;
    println!("set user hooks");
    Ok(())
}

fn get_unit_ground_pos(lua: MizLua, name: &str) -> LuaResult<Vector2> {
    let pos = Unit::get_by_name(lua, name)?.as_object()?.get_point()?;
    Ok(Vector2::from(na::Vector2::new(pos.0.x, pos.0.z)))
}

fn return_lives(lua: MizLua, ctx: &mut Context, ts: DateTime<Utc>) {
    let db = &mut ctx.db;
    ctx.recently_landed.retain(|slot, (name, landed_ts)| {
        if ts - *landed_ts >= Duration::seconds(10) {
            let pos = match get_unit_ground_pos(lua, &**name) {
                Ok(pos) => pos,
                Err(_) => return false,
            };
            !db.land(slot.clone(), pos)
        } else {
            true
        }
    });
}

fn init_miz(lua: MizLua) -> LuaResult<()> {
    let ctx = unsafe { Context::get_mut() };
    println!("adding event handler");
    World::singleton(lua)?.add_event_handler(on_event)?;
    let sortie = Miz::singleton(lua)?.sortie()?;
    let path = match Env::singleton(lua)?.get_value_dict_by_key(sortie)?.as_str() {
        "" => return Err(err("missing sortie in miz file")),
        s => PathBuf::from(format_compact!("{}\\{}", Lfs::singleton(lua)?.writedir()?, s).as_str()),
    };
    let timer = Timer::singleton(lua)?;
    timer.schedule_function(timer.get_time()? + 1., mlua::Value::Nil, {
        let path = path.clone();
        move |lua, _, now| {
            let ts = Utc::now();
            let ctx = unsafe { Context::get_mut() };
            if let Err(e) = ctx.db.maybe_do_repairs(lua, &ctx.idx, ts) {
                println!("error doing repairs {:?}", e)
            }
            return_lives(lua, ctx, ts);
            if let Some(snap) = ctx.db.maybe_snapshot() {
                ctx.do_background_task(BgTask::SaveState(path.clone(), snap));
            }
            let net = Net::singleton(lua)?;
            for id in ctx.force_to_spectators.drain() {
                net.force_player_slot(id, Side::Neutral, SlotId::spectator())?
            }
            Ok(Some(now + 1.))
        }
    })?;
    println!("spawning");
    if !path.exists() {
        let cfg = Cfg::load(&path)?;
        ctx.db = Db::init(lua, cfg, &ctx.idx, &Miz::singleton(lua)?)?;
    } else {
        ctx.db = Db::load(&path)?;
    }
    ctx.respawn_groups(lua)?;
    println!("spawned");
    Ok(())
}

#[mlua::lua_module]
fn bflib(lua: &Lua) -> LuaResult<LuaTable> {
    dcso3::create_root_module(lua, init_hooks, init_miz)
}

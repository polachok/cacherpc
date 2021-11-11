use mlua::{Lua, LuaOptions, StdLib};

pub mod cli;
pub mod control;
pub mod filter;
pub mod metrics;
pub mod pubsub;
pub mod rpc;
pub mod rpc_monitor;
pub mod types;

use tracing::info;

const LUA_JSON: &str = include_str!("json.lua");

pub fn init_lua(rules: &str) -> Result<Lua, mlua::Error> {
    // if any of the lua preparation steps contain errors, then WAF will not be used
    let lua = Lua::new_with(
        StdLib::MATH | StdLib::STRING | StdLib::PACKAGE,
        LuaOptions::default(),
    )?;

    let func = lua.load(LUA_JSON).into_function()?;

    let _: mlua::Value<'_> = lua.load_from_function("json", func)?;

    let rules = lua.load(&rules).into_function()?;

    let _: mlua::Value<'_> = lua.load_from_function("waf", rules)?;

    info!("loaded WAF rules");
    Ok(lua)
}

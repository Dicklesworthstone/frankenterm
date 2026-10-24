//! `wezterm.serde.{json,yaml,toml}_{decode,encode}` and the older
//! `wezterm.json_parse` / `wezterm.json_encode` (WezTerm's serde-funcs,
//! ported to mlua 0.12).
use config::lua::mlua::{self, IntoLua, Lua, Value as LuaValue};
use config::lua::{get_or_create_module, get_or_create_sub_module};
use luahelper::lua_value_to_dynamic;
use serde_json::{Map, Value as JValue};
use std::collections::HashSet;
use wezterm_dynamic::{FromDynamic, Value as DynValue};

pub fn register(lua: &Lua) -> anyhow::Result<()> {
    let serde_mod = get_or_create_sub_module(lua, "serde")?;

    serde_mod.set("json_decode", lua.create_function(json_decode)?)?;
    serde_mod.set("yaml_decode", lua.create_function(yaml_decode)?)?;
    serde_mod.set("toml_decode", lua.create_function(toml_decode)?)?;

    serde_mod.set("json_encode", lua.create_function(json_encode)?)?;
    serde_mod.set("yaml_encode", lua.create_function(yaml_encode)?)?;
    serde_mod.set("toml_encode", lua.create_function(toml_encode)?)?;
    serde_mod.set(
        "json_encode_pretty",
        lua.create_function(json_encode_pretty)?,
    )?;
    serde_mod.set(
        "toml_encode_pretty",
        lua.create_function(toml_encode_pretty)?,
    )?;
    // No pretty YAML encoder: the default one is pretty already
    // (https://github.com/dtolnay/serde-yaml/issues/226).

    // The older names.
    let wezterm_mod = get_or_create_module(lua, "wezterm")?;
    wezterm_mod.set("json_parse", lua.create_function(json_decode)?)?;
    wezterm_mod.set("json_encode", lua.create_function(json_encode)?)?;

    Ok(())
}

fn external(err: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::external(format!("{err:#}"))
}

fn json_encode(_: &Lua, value: LuaValue) -> mlua::Result<String> {
    let json = lua_value_to_json_value(value, &mut HashSet::new())?;
    serde_json::to_string(&json).map_err(external)
}

fn json_encode_pretty(_: &Lua, value: LuaValue) -> mlua::Result<String> {
    let json = lua_value_to_json_value(value, &mut HashSet::new())?;
    serde_json::to_string_pretty(&json).map_err(external)
}

fn yaml_encode(_: &Lua, value: LuaValue) -> mlua::Result<String> {
    let json = lua_value_to_json_value(value, &mut HashSet::new())?;
    serde_yaml::to_string(&json).map_err(external)
}

fn toml_encode(_: &Lua, value: LuaValue) -> mlua::Result<String> {
    let json = lua_value_to_json_value(value, &mut HashSet::new())?;
    toml::to_string(&json).map_err(external)
}

fn toml_encode_pretty(_: &Lua, value: LuaValue) -> mlua::Result<String> {
    let json = lua_value_to_json_value(value, &mut HashSet::new())?;
    toml::to_string_pretty(&json).map_err(external)
}

fn json_decode(lua: &Lua, text: String) -> mlua::Result<LuaValue> {
    let value = serde_json::from_str(&text).map_err(external)?;
    json_value_to_lua_value(lua, value)
}

fn yaml_decode(lua: &Lua, text: String) -> mlua::Result<LuaValue> {
    let value: JValue = serde_yaml::from_str(&text).map_err(external)?;
    json_value_to_lua_value(lua, value)
}

fn toml_decode(lua: &Lua, text: String) -> mlua::Result<LuaValue> {
    let value: JValue = toml::from_str(&text).map_err(external)?;
    json_value_to_lua_value(lua, value)
}

fn json_value_to_lua_value(lua: &Lua, value: JValue) -> mlua::Result<LuaValue> {
    Ok(match value {
        JValue::Null => LuaValue::Nil,
        JValue::Bool(b) => LuaValue::Boolean(b),
        JValue::Number(n) => match n.as_i64() {
            Some(n) => LuaValue::Integer(n),
            None => match n.as_f64() {
                Some(n) => LuaValue::Number(n),
                None => {
                    return Err(mlua::Error::external(format!(
                        "cannot represent {n:#?} as either i64 or f64"
                    )))
                }
            },
        },
        JValue::String(s) => s.into_lua(lua)?,
        JValue::Array(arr) => {
            let tbl = lua.create_table_with_capacity(arr.len(), 0)?;
            for (idx, value) in arr.into_iter().enumerate() {
                tbl.set(idx + 1, json_value_to_lua_value(lua, value)?)?;
            }
            LuaValue::Table(tbl)
        }
        JValue::Object(map) => {
            let tbl = lua.create_table_with_capacity(0, map.len())?;
            for (key, value) in map.into_iter() {
                tbl.set(key, json_value_to_lua_value(lua, value)?)?;
            }
            LuaValue::Table(tbl)
        }
    })
}

fn dyn_to_json(value: DynValue) -> anyhow::Result<JValue> {
    Ok(match value {
        DynValue::Null => JValue::Null,
        DynValue::Bool(b) => JValue::Bool(b),
        DynValue::String(s) => JValue::String(s),
        DynValue::Array(a) => JValue::Array(
            a.into_iter()
                .map(dyn_to_json)
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        DynValue::Object(o) => {
            let mut result = Map::new();
            for (k, v) in o {
                let k = String::from_dynamic(&k, Default::default())?;
                result.insert(k, dyn_to_json(v)?);
            }
            JValue::Object(result)
        }
        DynValue::U64(u) => JValue::Number(u.into()),
        DynValue::I64(u) => JValue::Number(u.into()),
        DynValue::F64(u) => JValue::Number(
            serde_json::Number::from_f64(*u)
                .ok_or_else(|| anyhow::anyhow!("number {u:?} cannot be represented in json"))?,
        ),
    })
}

fn conversion_error(from: &'static str, to: &str, message: Option<String>) -> mlua::Error {
    mlua::Error::FromLuaConversionError {
        from,
        to: to.to_string(),
        message,
    }
}

fn lua_value_to_json_value(value: LuaValue, visited: &mut HashSet<usize>) -> mlua::Result<JValue> {
    if let LuaValue::Table(_) = &value {
        // A table seen before (a cycle, or shared) encodes as null.
        if !visited.insert(value.to_pointer() as usize) {
            return Ok(JValue::Null);
        }
    }
    Ok(match value {
        LuaValue::Nil => JValue::Null,
        LuaValue::String(s) => JValue::String(s.to_str()?.to_string()),
        LuaValue::Boolean(b) => JValue::Bool(b),
        LuaValue::Integer(i) => JValue::Number(i.into()),
        LuaValue::Number(i) => match serde_json::value::Number::from_f64(i) {
            Some(n) => JValue::Number(n),
            None => {
                return Err(conversion_error(
                    "number",
                    "JsonValue",
                    Some(format!("unable to represent {i} as json float")),
                ))
            }
        },
        LuaValue::UserData(ud) => {
            let mt = ud.metatable().map_err(|err| {
                conversion_error(
                    "userdata",
                    "wezterm_dynamic::Value",
                    Some(format!("error getting metatable: {err:#}")),
                )
            })?;
            let to_dynamic = mt
                .get::<mlua::Function>("__wezterm_to_dynamic")
                .map_err(|_| {
                    conversion_error(
                        "userdata",
                        "wezterm_dynamic::Value",
                        Some("no __wezterm_to_dynamic metadata".to_string()),
                    )
                })?;
            let value = to_dynamic
                .call::<LuaValue>(LuaValue::UserData(ud.clone()))
                .map_err(|err| {
                    conversion_error(
                        "userdata",
                        "wezterm_dynamic::Value",
                        Some(format!("error calling __wezterm_to_dynamic: {err:#}")),
                    )
                })?;
            return dyn_to_json(lua_value_to_dynamic(value)?).map_err(mlua::Error::external);
        }
        // The special null light userdata.
        LuaValue::LightUserData(ud) if ud.0.is_null() => JValue::Null,
        LuaValue::Error(e) => return Err(*e),
        LuaValue::Table(table) => {
            if let Ok(true) = table.contains_key(1) {
                let mut array = vec![];
                for value in table.sequence_values::<LuaValue>() {
                    array.push(lua_value_to_json_value(value?, visited)?);
                }
                for pair in table.pairs::<LuaValue, LuaValue>() {
                    let (key, _value) = pair?;
                    match &key {
                        LuaValue::Integer(n) if *n >= 1 && *n as usize <= array.len() => {}
                        _ => {
                            let type_name = key.type_name();
                            let key = luahelper::ValuePrinter(key);
                            return Err(conversion_error(
                                type_name,
                                "numeric array index",
                                Some(format!("Unexpected key {key:?} for array style table")),
                            ));
                        }
                    }
                }
                JValue::Array(array)
            } else {
                let mut obj = Map::default();
                for pair in table.pairs::<LuaValue, LuaValue>() {
                    let (key, value) = pair?;
                    let key_type = key.type_name();
                    let key = match lua_value_to_json_value(key, visited)? {
                        JValue::String(s) => s,
                        _ => {
                            return Err(conversion_error(
                                key_type,
                                "string",
                                Some("json object keys must be strings".to_string()),
                            ));
                        }
                    };
                    let lua_type = value.type_name();
                    let value = lua_value_to_json_value(value, visited).map_err(|e| {
                        conversion_error(
                            lua_type,
                            "value",
                            Some(format!("while processing {key:?}: {e}")),
                        )
                    })?;
                    obj.insert(key, value);
                }
                JValue::Object(obj)
            }
        }
        other => return Err(conversion_error(other.type_name(), "JsonValue", None)),
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use serde_json::json;

    fn sample() -> JValue {
        json!({
            "key2str": "value1", "key2int": 4, "key2float": 4.5,
            "key2arr": vec![2, 3], "key2dict": {"a": "a_value", "b": 3}})
    }

    // Each converts a JSON value to Lua, encodes and decodes it, and compares
    // the result with the input.
    #[test]
    fn test_json_encode_decode() {
        let j0 = sample();
        let lua = Lua::new();
        let v0 = json_value_to_lua_value(&lua, j0.clone()).unwrap();
        for s in [
            json_encode(&lua, v0.clone()).unwrap(),
            json_encode_pretty(&lua, v0.clone()).unwrap(),
        ] {
            let j1: JValue = serde_json::from_str(&s).unwrap();
            assert_eq!(j0, j1);
            let v1 = json_decode(&lua, s).unwrap();
            assert_eq!(
                j0,
                lua_value_to_json_value(v1, &mut HashSet::new()).unwrap()
            );
        }
    }

    #[test]
    fn test_yaml_encode_decode() {
        let j0 = sample();
        let lua = Lua::new();
        let v0 = json_value_to_lua_value(&lua, j0.clone()).unwrap();
        let s = yaml_encode(&lua, v0).unwrap();
        let j1: JValue = serde_yaml::from_str(&s).unwrap();
        assert_eq!(j0, j1);
        let v1 = yaml_decode(&lua, s).unwrap();
        assert_eq!(
            j0,
            lua_value_to_json_value(v1, &mut HashSet::new()).unwrap()
        );
    }

    #[test]
    fn test_toml_encode_decode() {
        let j0 = sample();
        let lua = Lua::new();
        let v0 = json_value_to_lua_value(&lua, j0.clone()).unwrap();
        for s in [
            toml_encode(&lua, v0.clone()).unwrap(),
            toml_encode_pretty(&lua, v0.clone()).unwrap(),
        ] {
            let j1: JValue = toml::from_str(&s).unwrap();
            assert_eq!(j0, j1);
            let v1 = toml_decode(&lua, s).unwrap();
            assert_eq!(
                j0,
                lua_value_to_json_value(v1, &mut HashSet::new()).unwrap()
            );
        }
    }

    #[test]
    fn older_json_names_are_registered() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let (n, s): (i64, String) = lua
            .load(
                r#"
                local w = require('wezterm')
                local t = w.json_parse('{"n": 7}')
                return t.n, w.json_encode({ 1, 2 })
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!((n, s.as_str()), (7, "[1,2]"));
    }
}

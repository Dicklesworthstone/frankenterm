//! `wezterm.GLOBAL`: a table shared by every Lua context of the process,
//! which keeps its contents across configuration reloads (WezTerm's
//! share-data, ported to mlua 0.12).
//!
//! An upstream bug is fixed here: assigning to an array's last element
//! appended a copy instead of replacing it, and appending at `#arr + 1` was
//! refused.
use config::lua::get_or_create_module;
use config::lua::mlua::{
    self, IntoLua, Lua, UserData, UserDataMethods, UserDataRef, Value as LuaValue,
};
use ordered_float::OrderedFloat;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while a Lua callback held the lock leaves plain data behind;
    // keep serving it rather than poisoning every later configuration.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Clone)]
struct Object {
    inner: Arc<Mutex<BTreeMap<String, Value>>>,
}

impl Ord for Object {
    fn cmp(&self, other: &Self) -> Ordering {
        Arc::as_ptr(&self.inner).cmp(&Arc::as_ptr(&other.inner))
    }
}

impl PartialOrd for Object {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Object {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner) || *lock(&self.inner) == *lock(&other.inner)
    }
}

impl Eq for Object {}

impl Hash for Object {
    fn hash<H: Hasher>(&self, state: &mut H) {
        lock(&self.inner).hash(state)
    }
}

#[derive(Debug, Clone)]
struct Array {
    inner: Arc<Mutex<Vec<Value>>>,
}

impl Ord for Array {
    fn cmp(&self, other: &Self) -> Ordering {
        Arc::as_ptr(&self.inner).cmp(&Arc::as_ptr(&other.inner))
    }
}

impl PartialOrd for Array {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Array {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner) || *lock(&self.inner) == *lock(&other.inner)
    }
}

impl Eq for Array {}

impl Hash for Array {
    fn hash<H: Hasher>(&self, state: &mut H) {
        lock(&self.inner).hash(state)
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone)]
enum Value {
    Null,
    Bool(bool),
    String(String),
    Array(Array),
    Object(Object),
    I64(i64),
    F64(OrderedFloat<f64>),
}

fn conversion_error(from: &'static str, message: Option<String>) -> mlua::Error {
    mlua::Error::FromLuaConversionError {
        from,
        to: "wezterm.GLOBAL value".to_string(),
        message,
    }
}

fn lua_value_to_gvalue(value: LuaValue) -> mlua::Result<Value> {
    lua_value_to_gvalue_impl(value, &mut HashSet::new())
}

fn lua_value_to_gvalue_impl(value: LuaValue, visited: &mut HashSet<usize>) -> mlua::Result<Value> {
    if let LuaValue::Table(_) = &value {
        // A table seen before (a cycle, or shared) is stored as null.
        if !visited.insert(value.to_pointer() as usize) {
            return Ok(Value::Null);
        }
    }
    Ok(match value {
        LuaValue::Nil => Value::Null,
        LuaValue::String(s) => Value::String(s.to_str()?.to_string()),
        LuaValue::Boolean(b) => Value::Bool(b),
        LuaValue::Integer(i) => Value::I64(i),
        LuaValue::Number(n) => Value::F64(n.into()),
        // The special null light userdata.
        LuaValue::LightUserData(ud) if ud.0.is_null() => Value::Null,
        LuaValue::LightUserData(_) => return Err(conversion_error("lightuserdata", None)),
        LuaValue::UserData(ud) => {
            let mt = ud.metatable().map_err(|err| {
                conversion_error(
                    "userdata",
                    Some(format!("error getting metatable: {err:#}")),
                )
            })?;
            if let Ok(to_dynamic) = mt.get::<mlua::Function>("__wezterm_to_dynamic") {
                let value = to_dynamic
                    .call::<LuaValue>(LuaValue::UserData(ud.clone()))
                    .map_err(|err| {
                        conversion_error(
                            "userdata",
                            Some(format!("error calling __wezterm_to_dynamic: {err:#}")),
                        )
                    })?;
                return lua_value_to_gvalue_impl(value, visited);
            }
            let to_string = mt
                .get::<mlua::Function>(mlua::MetaMethod::ToString.name())
                .map_err(|err| {
                    conversion_error("userdata", Some(format!("error getting tostring: {err:#}")))
                })?;
            let value = to_string
                .call::<LuaValue>(LuaValue::UserData(ud.clone()))
                .map_err(|err| {
                    conversion_error("userdata", Some(format!("error calling tostring: {err:#}")))
                })?;
            return lua_value_to_gvalue_impl(value, visited);
        }
        LuaValue::Function(_) => return Err(conversion_error("function", None)),
        LuaValue::Thread(_) => return Err(conversion_error("thread", None)),
        LuaValue::Error(e) => return Err(*e),
        LuaValue::Table(table) => {
            if let Ok(true) = table.contains_key(1) {
                let mut array = vec![];
                for value in table.sequence_values::<LuaValue>() {
                    array.push(lua_value_to_gvalue_impl(value?, visited)?);
                }
                for pair in table.pairs::<LuaValue, LuaValue>() {
                    let (key, _) = pair?;
                    match &key {
                        LuaValue::Integer(n) if *n >= 1 && *n as usize <= array.len() => {}
                        _ => {
                            return Err(mlua::Error::FromLuaConversionError {
                                from: key.type_name(),
                                to: "numeric array index".to_string(),
                                message: Some(format!(
                                    "Unexpected key {key:?} for array style table"
                                )),
                            })
                        }
                    }
                }
                Value::Array(Array {
                    inner: Arc::new(Mutex::new(array)),
                })
            } else {
                let mut obj = BTreeMap::default();
                for pair in table.pairs::<String, LuaValue>() {
                    let (key, value) = pair?;
                    let lua_type = value.type_name();
                    let value = lua_value_to_gvalue_impl(value, visited).map_err(|e| {
                        mlua::Error::FromLuaConversionError {
                            from: lua_type,
                            to: "value".to_string(),
                            message: Some(format!("while processing {key:?}: {e}")),
                        }
                    })?;
                    obj.insert(key, value);
                }
                Value::Object(Object {
                    inner: Arc::new(Mutex::new(obj)),
                })
            }
        }
        other => return Err(conversion_error(other.type_name(), None)),
    })
}

static GLOBALS: LazyLock<Value> = LazyLock::new(|| {
    Value::Object(Object {
        inner: Arc::new(Mutex::new(BTreeMap::new())),
    })
});

/// A deep copy of `value` as plain Lua tables.
fn gvalue_to_lua(lua: &Lua, value: &Value) -> mlua::Result<LuaValue> {
    match value {
        Value::Array(arr) => {
            let result = lua.create_table()?;
            for (idx, value) in lock(&arr.inner).iter().enumerate() {
                result.set(idx + 1, gvalue_to_lua(lua, value)?)?;
            }
            Ok(LuaValue::Table(result))
        }
        Value::Object(obj) => {
            let result = lua.create_table()?;
            for (key, value) in lock(&obj.inner).iter() {
                result.set(key.clone(), gvalue_to_lua(lua, value)?)?;
            }
            Ok(LuaValue::Table(result))
        }
        scalar => element_to_lua(lua, scalar),
    }
}

/// An element as Lua reads it through `GLOBAL`: scalars by value, arrays and
/// objects as shared handles, so writes through them reach the global.
fn element_to_lua(lua: &Lua, value: &Value) -> mlua::Result<LuaValue> {
    match value {
        Value::Null => Ok(LuaValue::Nil),
        Value::Bool(b) => Ok(LuaValue::Boolean(*b)),
        Value::String(s) => s.as_str().into_lua(lua),
        Value::I64(i) => Ok(LuaValue::Integer(*i)),
        Value::F64(n) => Ok(LuaValue::Number(n.into_inner())),
        Value::Array(_) | Value::Object(_) => value.clone().into_lua(lua),
    }
}

/// The zero-based slot of a Lua array index, or an error for 0 and below.
fn array_slot(i: i64) -> mlua::Result<usize> {
    usize::try_from(i)
        .ok()
        .and_then(|i| i.checked_sub(1))
        .ok_or_else(|| mlua::Error::external(format!("invalid array index {i}")))
}

impl UserData for Value {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method("__wezterm_to_dynamic", |lua, this, ()| {
            gvalue_to_lua(lua, this)
        });
        methods.add_meta_method(mlua::MetaMethod::Len, |lua, this, ()| match this {
            Value::Array(arr) => lock(&arr.inner).len().into_lua(lua),
            Value::Object(obj) => lock(&obj.inner).len().into_lua(lua),
            Value::String(s) => s.len().into_lua(lua),
            _ => Err(mlua::Error::external("invalid type for len operator")),
        });

        methods.add_meta_method(mlua::MetaMethod::Pairs, |lua, this, ()| match this {
            Value::Array(_) => {
                let iter = lua.create_function(
                    |lua,
                     (this, i): (UserDataRef<Value>, usize)|
                     -> mlua::Result<mlua::MultiValue> {
                        let Value::Array(arr) = &*this else {
                            return Ok(mlua::MultiValue::new());
                        };
                        let arr = lock(&arr.inner);
                        match arr.get(i) {
                            Some(value) => Ok(mlua::MultiValue::from_vec(vec![
                                (i + 1).into_lua(lua)?,
                                element_to_lua(lua, value)?,
                            ])),
                            None => Ok(mlua::MultiValue::new()),
                        }
                    },
                )?;
                Ok((iter, this.clone(), 0.into_lua(lua)?))
            }
            Value::Object(_) => {
                let iter = lua.create_function(
                    |lua,
                     (this, key): (UserDataRef<Value>, Option<String>)|
                     -> mlua::Result<mlua::MultiValue> {
                        let Value::Object(obj) = &*this else {
                            return Ok(mlua::MultiValue::new());
                        };
                        let obj = lock(&obj.inner);
                        let next = match &key {
                            None => obj.iter().next(),
                            Some(key) => obj
                                .range::<String, _>((
                                    std::ops::Bound::Excluded(key),
                                    std::ops::Bound::Unbounded,
                                ))
                                .next(),
                        };
                        match next {
                            Some((key, value)) => Ok(mlua::MultiValue::from_vec(vec![
                                key.as_str().into_lua(lua)?,
                                element_to_lua(lua, value)?,
                            ])),
                            None => Ok(mlua::MultiValue::new()),
                        }
                    },
                )?;
                Ok((iter, this.clone(), LuaValue::Nil))
            }
            _ => Err(mlua::Error::external("invalid type for __pairs metamethod")),
        });

        methods.add_meta_method(
            mlua::MetaMethod::Index,
            |lua, this, key: LuaValue| -> mlua::Result<LuaValue> {
                match (this, key) {
                    (Value::Array(arr), LuaValue::Integer(i)) => {
                        let slot = array_slot(i)?;
                        match lock(&arr.inner).get(slot) {
                            Some(value) => element_to_lua(lua, value),
                            None => Ok(LuaValue::Nil),
                        }
                    }
                    (Value::Array(_), _) => Err(mlua::Error::external(
                        "can only index arrays using integer values",
                    )),
                    (Value::Object(obj), LuaValue::String(s)) => {
                        let s = s.to_str().map_err(|e| {
                            mlua::Error::external(format!(
                                "can only index objects using unicode strings: {e:#}"
                            ))
                        })?;
                        match lock(&obj.inner).get(&*s) {
                            Some(value) => element_to_lua(lua, value),
                            None => Ok(LuaValue::Nil),
                        }
                    }
                    (Value::Object(_), _) => Err(mlua::Error::external(
                        "can only index objects using string values",
                    )),
                    _ => Err(mlua::Error::external(
                        "can only index array or object values",
                    )),
                }
            },
        );
        methods.add_meta_method(
            mlua::MetaMethod::NewIndex,
            |_, this, (key, value): (LuaValue, LuaValue)| -> mlua::Result<()> {
                match (this, key) {
                    (Value::Array(arr), LuaValue::Integer(i)) => {
                        let slot = array_slot(i)?;
                        let value = lua_value_to_gvalue(value)?;
                        let mut arr = lock(&arr.inner);
                        if slot < arr.len() {
                            arr[slot] = value;
                        } else if slot == arr.len() {
                            arr.push(value);
                        } else {
                            return Err(mlua::Error::external(format!(
                                "cannot make a sparse array by setting index {i} when its length is {}",
                                arr.len()
                            )));
                        }
                        Ok(())
                    }
                    (Value::Array(_), _) => Err(mlua::Error::external(
                        "can only index arrays using integer values",
                    )),
                    (Value::Object(obj), LuaValue::String(s)) => {
                        let s = s.to_str().map_err(|e| {
                            mlua::Error::external(format!(
                                "can only index objects using unicode strings: {e:#}"
                            ))
                        })?;
                        let value = lua_value_to_gvalue(value)?;
                        lock(&obj.inner).insert(s.to_string(), value);
                        Ok(())
                    }
                    (Value::Object(_), _) => Err(mlua::Error::external(
                        "can only index objects using string values",
                    )),
                    _ => Err(mlua::Error::external(
                        "can only index array or object values",
                    )),
                }
            },
        );
    }
}

pub fn register(lua: &Lua) -> anyhow::Result<()> {
    let wezterm_mod = get_or_create_module(lua, "wezterm")?;
    wezterm_mod.set("GLOBAL", GLOBALS.clone())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua() -> Lua {
        let lua = Lua::new();
        register(&lua).expect("register share-data");
        lua
    }

    #[test]
    fn global_survives_a_new_lua_context() {
        lua()
            .load("require('wezterm').GLOBAL.test_count = 41")
            .exec()
            .expect("set");
        let count: i64 = lua()
            .load(
                "local g = require('wezterm').GLOBAL; g.test_count = g.test_count + 1; return g.test_count",
            )
            .eval()
            .expect("read back in a new context");
        assert_eq!(count, 42);
    }

    #[test]
    fn arrays_replace_and_append_and_refuse_holes() {
        let lua = lua();
        let (len, first, last, n): (i64, String, String, i64) = lua
            .load(
                r#"
                local g = require('wezterm').GLOBAL
                g.test_list = { 'a', 'b' }
                local list = g.test_list
                list[2] = 'B'    -- replaces the last element
                list[3] = 'c'    -- appends
                local n = 0
                for _ in pairs(list) do n = n + 1 end
                return #list, list[1], list[3], n
            "#,
            )
            .eval()
            .expect("array writes");
        assert_eq!((len, first.as_str(), last.as_str(), n), (3, "a", "c", 3));
        let second: String = lua
            .load("return require('wezterm').GLOBAL.test_list[2]")
            .eval()
            .expect("replaced element");
        assert_eq!(second, "B");
        let err = lua
            .load("require('wezterm').GLOBAL.test_list[9] = 'x'")
            .exec()
            .expect_err("a hole");
        assert!(err.to_string().contains("sparse"), "{}", err);
    }

    #[test]
    fn objects_iterate_in_key_order_and_strings_have_a_length() {
        let lua = lua();
        let (keys, len): (String, i64) = lua
            .load(
                r#"
                local g = require('wezterm').GLOBAL
                g.test_obj = { b = 2, a = 1, c = 'xyz' }
                local keys = {}
                for k, _ in pairs(g.test_obj) do table.insert(keys, k) end
                return table.concat(keys, ','), #g.test_obj
            "#,
            )
            .eval()
            .expect("object iteration");
        assert_eq!((keys.as_str(), len), ("a,b,c", 3));
    }
}

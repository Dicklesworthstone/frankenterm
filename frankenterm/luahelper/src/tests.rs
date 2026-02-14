use super::*;
use frankenterm_dynamic::Value as DynValue;
use ordered_float::OrderedFloat;
use std::collections::BTreeMap;

fn lua() -> mlua::Lua {
    mlua::Lua::new()
}

// ── dynamic_to_lua_value: scalars ──────────────────────────

#[test]
fn null_to_lua_is_nil() {
    let l = lua();
    let result = dynamic_to_lua_value(&l, DynValue::Null).unwrap();
    assert!(matches!(result, LuaValue::Nil));
}

#[test]
fn bool_true_to_lua() {
    let l = lua();
    let result = dynamic_to_lua_value(&l, DynValue::Bool(true)).unwrap();
    assert_eq!(result, LuaValue::Boolean(true));
}

#[test]
fn bool_false_to_lua() {
    let l = lua();
    let result = dynamic_to_lua_value(&l, DynValue::Bool(false)).unwrap();
    assert_eq!(result, LuaValue::Boolean(false));
}

#[test]
fn string_to_lua() {
    let l = lua();
    let result = dynamic_to_lua_value(&l, DynValue::String("hello".to_string())).unwrap();
    if let LuaValue::String(s) = result {
        assert_eq!(s.to_str().unwrap(), "hello");
    } else {
        panic!("{}", format!("expected String, got {result:?}"));
    }
}

#[test]
fn empty_string_to_lua() {
    let l = lua();
    let result = dynamic_to_lua_value(&l, DynValue::String(String::new())).unwrap();
    if let LuaValue::String(s) = result {
        assert_eq!(s.to_str().unwrap(), "");
    } else {
        panic!("expected String");
    }
}

#[test]
fn u64_to_lua() {
    let l = lua();
    let result = dynamic_to_lua_value(&l, DynValue::U64(42)).unwrap();
    match result {
        LuaValue::Integer(n) => assert_eq!(n, 42),
        LuaValue::Number(n) => assert_eq!(n, 42.0),
        _ => panic!("{}", format!("expected numeric, got {result:?}")),
    }
}

#[test]
fn i64_to_lua() {
    let l = lua();
    let result = dynamic_to_lua_value(&l, DynValue::I64(-7)).unwrap();
    match result {
        LuaValue::Integer(n) => assert_eq!(n, -7),
        LuaValue::Number(n) => assert_eq!(n, -7.0),
        _ => panic!("{}", format!("expected numeric, got {result:?}")),
    }
}

#[test]
fn f64_to_lua() {
    let l = lua();
    let result = dynamic_to_lua_value(&l, DynValue::F64(OrderedFloat(3.14))).unwrap();
    match result {
        LuaValue::Number(n) => assert!((n - 3.14).abs() < 1e-10),
        _ => panic!("{}", format!("expected Number, got {result:?}")),
    }
}

// ── dynamic_to_lua_value: arrays ───────────────────────────

#[test]
fn array_to_lua_table() {
    let l = lua();
    let arr: frankenterm_dynamic::Array =
        vec![DynValue::U64(10), DynValue::U64(20), DynValue::U64(30)].into();
    let result = dynamic_to_lua_value(&l, DynValue::Array(arr)).unwrap();
    if let LuaValue::Table(t) = result {
        // Lua arrays are 1-indexed
        assert_eq!(t.get::<_, i64>(1).unwrap(), 10);
        assert_eq!(t.get::<_, i64>(2).unwrap(), 20);
        assert_eq!(t.get::<_, i64>(3).unwrap(), 30);
    } else {
        panic!("expected Table");
    }
}

#[test]
fn empty_array_to_lua() {
    let l = lua();
    let arr: frankenterm_dynamic::Array = vec![].into();
    let result = dynamic_to_lua_value(&l, DynValue::Array(arr)).unwrap();
    if let LuaValue::Table(t) = result {
        assert_eq!(t.len().unwrap(), 0);
    } else {
        panic!("expected Table");
    }
}

// ── dynamic_to_lua_value: objects ──────────────────────────

#[test]
fn object_to_lua_table() {
    let l = lua();
    let mut map = BTreeMap::new();
    map.insert(
        DynValue::String("key".to_string()),
        DynValue::String("value".to_string()),
    );
    let obj: frankenterm_dynamic::Object = map.into();
    let result = dynamic_to_lua_value(&l, DynValue::Object(obj)).unwrap();
    if let LuaValue::Table(t) = result {
        let val: String = t.get::<_, String>("key").unwrap();
        assert_eq!(val, "value");
    } else {
        panic!("expected Table");
    }
}

#[test]
fn empty_object_to_lua() {
    let l = lua();
    let obj: frankenterm_dynamic::Object = BTreeMap::new().into();
    let result = dynamic_to_lua_value(&l, DynValue::Object(obj)).unwrap();
    assert!(matches!(result, LuaValue::Table(_)));
}

// ── lua_value_to_dynamic: scalars ──────────────────────────

#[test]
fn nil_to_dynamic_is_null() {
    let result = lua_value_to_dynamic(LuaValue::Nil).unwrap();
    assert_eq!(result, DynValue::Null);
}

#[test]
fn lua_bool_to_dynamic() {
    assert_eq!(
        lua_value_to_dynamic(LuaValue::Boolean(true)).unwrap(),
        DynValue::Bool(true)
    );
    assert_eq!(
        lua_value_to_dynamic(LuaValue::Boolean(false)).unwrap(),
        DynValue::Bool(false)
    );
}

#[test]
fn lua_integer_to_dynamic() {
    let result = lua_value_to_dynamic(LuaValue::Integer(42)).unwrap();
    assert_eq!(result, DynValue::I64(42));
}

#[test]
fn lua_negative_integer_to_dynamic() {
    let result = lua_value_to_dynamic(LuaValue::Integer(-100)).unwrap();
    assert_eq!(result, DynValue::I64(-100));
}

#[test]
fn lua_number_to_dynamic() {
    let result = lua_value_to_dynamic(LuaValue::Number(2.718)).unwrap();
    assert_eq!(result, DynValue::F64(OrderedFloat(2.718)));
}

#[test]
fn lua_string_to_dynamic() {
    let l = lua();
    let s = l.create_string("hello").unwrap();
    let result = lua_value_to_dynamic(LuaValue::String(s)).unwrap();
    assert_eq!(result, DynValue::String("hello".to_string()));
}

// ── lua_value_to_dynamic: tables ───────────────────────────

#[test]
fn lua_array_table_to_dynamic() {
    let l = lua();
    let t = l.create_table().unwrap();
    t.set(1, "a").unwrap();
    t.set(2, "b").unwrap();
    t.set(3, "c").unwrap();

    let result = lua_value_to_dynamic(LuaValue::Table(t)).unwrap();
    if let DynValue::Array(arr) = result {
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0], DynValue::String("a".to_string()));
        assert_eq!(arr[1], DynValue::String("b".to_string()));
        assert_eq!(arr[2], DynValue::String("c".to_string()));
    } else {
        panic!("{}", format!("expected Array, got {result:?}"));
    }
}

#[test]
fn lua_object_table_to_dynamic() {
    let l = lua();
    let t = l.create_table().unwrap();
    t.set("name", "test").unwrap();
    t.set("count", 5).unwrap();

    let result = lua_value_to_dynamic(LuaValue::Table(t)).unwrap();
    if let DynValue::Object(obj) = result {
        assert_eq!(
            obj.get_by_str("name"),
            Some(&DynValue::String("test".to_string()))
        );
        assert_eq!(obj.get_by_str("count"), Some(&DynValue::I64(5)));
    } else {
        panic!("{}", format!("expected Object, got {result:?}"));
    }
}

#[test]
fn lua_empty_table_to_dynamic_is_object() {
    let l = lua();
    let t = l.create_table().unwrap();

    let result = lua_value_to_dynamic(LuaValue::Table(t)).unwrap();
    // Empty table has no key=1, so treated as object
    assert!(matches!(result, DynValue::Object(_)));
}

#[test]
fn lua_nested_table_to_dynamic() {
    let l = lua();
    let inner = l.create_table().unwrap();
    inner.set("nested", true).unwrap();

    let outer = l.create_table().unwrap();
    outer.set("child", inner).unwrap();

    let result = lua_value_to_dynamic(LuaValue::Table(outer)).unwrap();
    if let DynValue::Object(obj) = result {
        if let Some(DynValue::Object(inner)) = obj.get_by_str("child") {
            assert_eq!(inner.get_by_str("nested"), Some(&DynValue::Bool(true)));
        } else {
            panic!("expected nested Object");
        }
    } else {
        panic!("expected Object");
    }
}

// ── lua_value_to_dynamic: error cases ──────────────────────

#[test]
fn lua_function_to_dynamic_fails() {
    let l = lua();
    let func = l.create_function(|_, ()| Ok(())).unwrap();
    let result = lua_value_to_dynamic(LuaValue::Function(func));
    assert!(result.is_err());
}

#[test]
fn lua_null_light_userdata_is_null() {
    let result = lua_value_to_dynamic(LuaValue::LightUserData(mlua::LightUserData(
        std::ptr::null_mut(),
    )))
    .unwrap();
    assert_eq!(result, DynValue::Null);
}

#[test]
fn lua_non_null_light_userdata_fails() {
    // Use a non-null pointer
    let fake_ptr = 0x1 as *mut std::ffi::c_void;
    let result = lua_value_to_dynamic(LuaValue::LightUserData(mlua::LightUserData(fake_ptr)));
    assert!(result.is_err());
}

// ── roundtrip: dynamic -> lua -> dynamic ───────────────────

#[test]
fn roundtrip_null() {
    let l = lua();
    let original = DynValue::Null;
    let lua_val = dynamic_to_lua_value(&l, original.clone()).unwrap();
    let back = lua_value_to_dynamic(lua_val).unwrap();
    assert_eq!(back, original);
}

#[test]
fn roundtrip_bool() {
    let l = lua();
    for b in [true, false] {
        let original = DynValue::Bool(b);
        let lua_val = dynamic_to_lua_value(&l, original.clone()).unwrap();
        let back = lua_value_to_dynamic(lua_val).unwrap();
        assert_eq!(back, original);
    }
}

#[test]
fn roundtrip_string() {
    let l = lua();
    let original = DynValue::String("roundtrip test".to_string());
    let lua_val = dynamic_to_lua_value(&l, original.clone()).unwrap();
    let back = lua_value_to_dynamic(lua_val).unwrap();
    assert_eq!(back, original);
}

#[test]
fn roundtrip_i64() {
    let l = lua();
    let original = DynValue::I64(-42);
    let lua_val = dynamic_to_lua_value(&l, original.clone()).unwrap();
    let back = lua_value_to_dynamic(lua_val).unwrap();
    // May come back as I64 or F64 depending on Lua representation
    match back {
        DynValue::I64(n) => assert_eq!(n, -42),
        DynValue::F64(OrderedFloat(n)) => assert_eq!(n, -42.0),
        _other => panic!("unexpected roundtrip_i64 result"),
    }
}

#[test]
fn roundtrip_f64() {
    let l = lua();
    let original = DynValue::F64(OrderedFloat(1.5));
    let lua_val = dynamic_to_lua_value(&l, original.clone()).unwrap();
    let back = lua_value_to_dynamic(lua_val).unwrap();
    match back {
        DynValue::F64(OrderedFloat(n)) => assert!((n - 1.5).abs() < 1e-10),
        _other => panic!("unexpected roundtrip_f64 result"),
    }
}

#[test]
fn roundtrip_array() {
    let l = lua();
    let arr: frankenterm_dynamic::Array = vec![
        DynValue::String("a".to_string()),
        DynValue::String("b".to_string()),
    ]
    .into();
    let original = DynValue::Array(arr);
    let lua_val = dynamic_to_lua_value(&l, original.clone()).unwrap();
    let back = lua_value_to_dynamic(lua_val).unwrap();
    assert_eq!(back, original);
}

#[test]
fn roundtrip_object() {
    let l = lua();
    let mut map = BTreeMap::new();
    map.insert(DynValue::String("x".to_string()), DynValue::Bool(true));
    let obj: frankenterm_dynamic::Object = map.into();
    let original = DynValue::Object(obj);
    let lua_val = dynamic_to_lua_value(&l, original.clone()).unwrap();
    let back = lua_value_to_dynamic(lua_val).unwrap();
    assert_eq!(back, original);
}

// ── is_array_style_table ───────────────────────────────────

#[test]
fn array_style_table_detection() {
    let l = lua();
    let t = l.create_table().unwrap();
    t.set(1, "a").unwrap();
    t.set(2, "b").unwrap();
    assert!(is_array_style_table(&t));
}

#[test]
fn object_style_table_detection() {
    let l = lua();
    let t = l.create_table().unwrap();
    t.set("key", "value").unwrap();
    assert!(!is_array_style_table(&t));
}

#[test]
fn empty_table_is_array_style() {
    let l = lua();
    let t = l.create_table().unwrap();
    assert!(is_array_style_table(&t));
}

#[test]
fn non_contiguous_keys_not_array() {
    let l = lua();
    let t = l.create_table().unwrap();
    t.set(1, "a").unwrap();
    t.set(3, "c").unwrap(); // gap at 2
    assert!(!is_array_style_table(&t));
}

#[test]
fn mixed_keys_not_array() {
    let l = lua();
    let t = l.create_table().unwrap();
    t.set(1, "a").unwrap();
    t.set("extra", "b").unwrap();
    assert!(!is_array_style_table(&t));
}

// ── circular reference handling ────────────────────────────

#[test]
fn circular_table_becomes_null() {
    let l = lua();
    let t = l.create_table().unwrap();
    t.set("self", t.clone()).unwrap();

    let result = lua_value_to_dynamic(LuaValue::Table(t)).unwrap();
    // The circular reference should be replaced with Null
    if let DynValue::Object(obj) = result {
        assert_eq!(obj.get_by_str("self"), Some(&DynValue::Null));
    } else {
        panic!("expected Object");
    }
}

// ── ValuePrinter ───────────────────────────────────────────

#[test]
fn value_printer_nil() {
    let debug = format!("{:?}", ValuePrinter(LuaValue::Nil));
    assert_eq!(debug, "nil");
}

#[test]
fn value_printer_bool() {
    let debug = format!("{:?}", ValuePrinter(LuaValue::Boolean(true)));
    assert_eq!(debug, "true");
}

#[test]
fn value_printer_integer() {
    let debug = format!("{:?}", ValuePrinter(LuaValue::Integer(42)));
    assert_eq!(debug, "42");
}

#[test]
fn value_printer_string() {
    let l = lua();
    let s = l.create_string("test").unwrap();
    let debug = format!("{:?}", ValuePrinter(LuaValue::String(s)));
    assert!(debug.contains("test"));
}

// ── Additional dynamic_to_lua_value tests ──────────────────

#[test]
fn u64_zero_to_lua() {
    let l = lua();
    let result = dynamic_to_lua_value(&l, DynValue::U64(0)).unwrap();
    match result {
        LuaValue::Integer(n) => assert_eq!(n, 0),
        LuaValue::Number(n) => assert_eq!(n, 0.0),
        _ => panic!("expected numeric"),
    }
}

#[test]
fn i64_zero_to_lua() {
    let l = lua();
    let result = dynamic_to_lua_value(&l, DynValue::I64(0)).unwrap();
    match result {
        LuaValue::Integer(n) => assert_eq!(n, 0),
        _ => panic!("expected integer"),
    }
}

#[test]
fn unicode_string_to_lua() {
    let l = lua();
    let result =
        dynamic_to_lua_value(&l, DynValue::String("hello \u{1F600} world".to_string())).unwrap();
    if let LuaValue::String(s) = result {
        assert_eq!(s.to_str().unwrap(), "hello \u{1F600} world");
    } else {
        panic!("expected String");
    }
}

#[test]
fn nested_array_to_lua() {
    let l = lua();
    let inner: frankenterm_dynamic::Array = vec![DynValue::U64(1), DynValue::U64(2)].into();
    let outer: frankenterm_dynamic::Array = vec![DynValue::Array(inner)].into();
    let result = dynamic_to_lua_value(&l, DynValue::Array(outer)).unwrap();
    if let LuaValue::Table(t) = result {
        let inner_table: mlua::Table = t.get(1).unwrap();
        assert_eq!(inner_table.get::<_, i64>(1).unwrap(), 1);
        assert_eq!(inner_table.get::<_, i64>(2).unwrap(), 2);
    } else {
        panic!("expected Table");
    }
}

#[test]
fn object_with_multiple_types_to_lua() {
    let l = lua();
    let mut map = BTreeMap::new();
    map.insert(
        DynValue::String("name".to_string()),
        DynValue::String("test".to_string()),
    );
    map.insert(DynValue::String("count".to_string()), DynValue::I64(42));
    map.insert(
        DynValue::String("active".to_string()),
        DynValue::Bool(true),
    );
    let obj: frankenterm_dynamic::Object = map.into();
    let result = dynamic_to_lua_value(&l, DynValue::Object(obj)).unwrap();
    if let LuaValue::Table(t) = result {
        assert_eq!(t.get::<_, String>("name").unwrap(), "test");
        assert_eq!(t.get::<_, i64>("count").unwrap(), 42);
        assert_eq!(t.get::<_, bool>("active").unwrap(), true);
    } else {
        panic!("expected Table");
    }
}

// ── Additional lua_value_to_dynamic tests ──────────────────

#[test]
fn lua_zero_integer_to_dynamic() {
    let result = lua_value_to_dynamic(LuaValue::Integer(0)).unwrap();
    assert_eq!(result, DynValue::I64(0));
}

#[test]
fn lua_large_integer_to_dynamic() {
    let result = lua_value_to_dynamic(LuaValue::Integer(i64::MAX)).unwrap();
    assert_eq!(result, DynValue::I64(i64::MAX));
}

#[test]
fn lua_min_integer_to_dynamic() {
    let result = lua_value_to_dynamic(LuaValue::Integer(i64::MIN)).unwrap();
    assert_eq!(result, DynValue::I64(i64::MIN));
}

#[test]
fn lua_negative_number_to_dynamic() {
    let result = lua_value_to_dynamic(LuaValue::Number(-3.14)).unwrap();
    if let DynValue::F64(f) = result {
        assert!((f.into_inner() + 3.14).abs() < 1e-10);
    } else {
        panic!("expected F64");
    }
}

#[test]
fn lua_nested_array_table_to_dynamic() {
    let l = lua();
    let inner = l.create_table().unwrap();
    inner.set(1, 10).unwrap();
    inner.set(2, 20).unwrap();

    let outer = l.create_table().unwrap();
    outer.set(1, inner).unwrap();

    let result = lua_value_to_dynamic(LuaValue::Table(outer)).unwrap();
    if let DynValue::Array(arr) = result {
        assert_eq!(arr.len(), 1);
        if let DynValue::Array(inner) = &arr[0] {
            assert_eq!(inner.len(), 2);
        } else {
            panic!("expected inner Array");
        }
    } else {
        panic!("expected Array");
    }
}

#[test]
fn lua_mixed_array_values_to_dynamic() {
    let l = lua();
    let t = l.create_table().unwrap();
    t.set(1, "hello").unwrap();
    t.set(2, 42).unwrap();
    t.set(3, true).unwrap();

    let result = lua_value_to_dynamic(LuaValue::Table(t)).unwrap();
    if let DynValue::Array(arr) = result {
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0], DynValue::String("hello".to_string()));
        assert_eq!(arr[1], DynValue::I64(42));
        assert_eq!(arr[2], DynValue::Bool(true));
    } else {
        panic!("expected Array");
    }
}

// ── Additional roundtrip tests ─────────────────────────────

#[test]
fn roundtrip_u64() {
    let l = lua();
    let original = DynValue::U64(999);
    let lua_val = dynamic_to_lua_value(&l, original).unwrap();
    let back = lua_value_to_dynamic(lua_val).unwrap();
    // U64 may roundtrip as I64 through Lua integers
    match back {
        DynValue::I64(n) => assert_eq!(n, 999),
        DynValue::U64(n) => assert_eq!(n, 999),
        DynValue::F64(OrderedFloat(n)) => assert_eq!(n, 999.0),
        _ => panic!("unexpected roundtrip_u64 result"),
    }
}

#[test]
fn roundtrip_nested_object() {
    let l = lua();
    let mut inner = BTreeMap::new();
    inner.insert(DynValue::String("a".to_string()), DynValue::I64(1));
    let mut outer = BTreeMap::new();
    outer.insert(
        DynValue::String("inner".to_string()),
        DynValue::Object(inner.into()),
    );
    let original = DynValue::Object(outer.into());
    let lua_val = dynamic_to_lua_value(&l, original.clone()).unwrap();
    let back = lua_value_to_dynamic(lua_val).unwrap();
    assert_eq!(back, original);
}

// ── Additional is_array_style_table tests ──────────────────

#[test]
fn single_element_array_is_array_style() {
    let l = lua();
    let t = l.create_table().unwrap();
    t.set(1, "only").unwrap();
    assert!(is_array_style_table(&t));
}

#[test]
fn zero_based_keys_not_array() {
    let l = lua();
    let t = l.create_table().unwrap();
    t.set(0, "zero").unwrap();
    t.set(1, "one").unwrap();
    assert!(!is_array_style_table(&t));
}

#[test]
fn negative_keys_not_array() {
    let l = lua();
    let t = l.create_table().unwrap();
    t.set(-1, "neg").unwrap();
    assert!(!is_array_style_table(&t));
}

// ── Additional ValuePrinter tests ──────────────────────────

#[test]
fn value_printer_number() {
    let debug = format!("{:?}", ValuePrinter(LuaValue::Number(3.14)));
    assert!(debug.contains("3.14"));
}

#[test]
fn value_printer_array_table() {
    let l = lua();
    let t = l.create_table().unwrap();
    t.set(1, "a").unwrap();
    t.set(2, "b").unwrap();
    let debug = format!("{:?}", ValuePrinter(LuaValue::Table(t)));
    assert!(debug.contains("a"));
    assert!(debug.contains("b"));
}

#[test]
fn value_printer_object_table() {
    let l = lua();
    let t = l.create_table().unwrap();
    t.set("key", "value").unwrap();
    let debug = format!("{:?}", ValuePrinter(LuaValue::Table(t)));
    assert!(debug.contains("key"));
    assert!(debug.contains("value"));
}

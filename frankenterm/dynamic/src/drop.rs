use crate::Value;

#[cfg(not(feature = "std"))]
extern crate alloc;

use alloc::vec::Vec;

/// Non-recursive drop implementation.
/// This is taken from dtolnay's miniserde library
/// and is reproduced here under the terms of its
/// MIT license
pub fn safely(value: Value) {
    match value {
        Value::Array(_) | Value::Object(_) => {}
        _ => return,
    }

    let mut stack = Vec::new();
    stack.push(value);
    while let Some(value) = stack.pop() {
        match value {
            Value::Array(vec) => {
                for child in vec {
                    stack.push(child);
                }
            }
            Value::Object(map) => {
                for (_, child) in map {
                    stack.push(child);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use crate::array::Array;
    use crate::object::Object;
    use alloc::collections::BTreeMap;
    use alloc::string::ToString;
    use alloc::vec;

    #[test]
    fn safely_drops_null() {
        safely(Value::Null);
    }

    #[test]
    fn safely_drops_bool() {
        safely(Value::Bool(true));
    }

    #[test]
    fn safely_drops_string() {
        safely(Value::String("test".to_string()));
    }

    #[test]
    fn safely_drops_u64() {
        safely(Value::U64(42));
    }

    #[test]
    fn safely_drops_i64() {
        safely(Value::I64(-1));
    }

    #[test]
    fn safely_drops_f64() {
        safely(Value::F64(ordered_float::OrderedFloat(2.72)));
    }

    #[test]
    fn safely_drops_empty_array() {
        safely(Value::Array(Array::new()));
    }

    #[test]
    fn safely_drops_flat_array() {
        let arr: Array = vec![Value::U64(1), Value::U64(2), Value::U64(3)].into();
        safely(Value::Array(arr));
    }

    #[test]
    fn safely_drops_empty_object() {
        let obj: Object = BTreeMap::new().into();
        safely(Value::Object(obj));
    }

    #[test]
    fn safely_drops_flat_object() {
        let mut map = BTreeMap::new();
        map.insert(Value::String("a".to_string()), Value::U64(1));
        map.insert(Value::String("b".to_string()), Value::Bool(true));
        let obj: Object = map.into();
        safely(Value::Object(obj));
    }

    #[test]
    fn safely_drops_nested_arrays() {
        let inner: Array = vec![Value::U64(42)].into();
        let outer: Array = vec![Value::Array(inner), Value::Null].into();
        safely(Value::Array(outer));
    }

    #[test]
    fn safely_drops_nested_objects() {
        let mut inner_map = BTreeMap::new();
        inner_map.insert(Value::String("x".to_string()), Value::U64(1));
        let inner: Object = inner_map.into();

        let mut outer_map = BTreeMap::new();
        outer_map.insert(Value::String("inner".to_string()), Value::Object(inner));
        let outer: Object = outer_map.into();

        safely(Value::Object(outer));
    }

    #[test]
    fn safely_drops_mixed_nested() {
        let arr: Array = vec![Value::U64(1), Value::String("hello".to_string())].into();
        let mut map = BTreeMap::new();
        map.insert(Value::String("arr".to_string()), Value::Array(arr));
        map.insert(Value::String("val".to_string()), Value::Bool(false));
        let obj: Object = map.into();

        let top: Array = vec![Value::Object(obj), Value::Null].into();
        safely(Value::Array(top));
    }

    #[test]
    fn safely_handles_deeply_nested_without_stack_overflow() {
        // Build a chain: Array([ Array([ Array([ ... Value::U64(0) ]) ]) ])
        let mut current = Value::U64(0);
        for _ in 0..1000 {
            let arr: Array = vec![current].into();
            current = Value::Array(arr);
        }
        safely(current);
    }

    #[test]
    fn safely_handles_deeply_nested_objects_without_stack_overflow() {
        let mut current = Value::U64(0);
        for i in 0..1000 {
            let mut map = BTreeMap::new();
            map.insert(Value::String(alloc::format!("level_{i}")), current);
            let obj: Object = map.into();
            current = Value::Object(obj);
        }
        safely(current);
    }
}

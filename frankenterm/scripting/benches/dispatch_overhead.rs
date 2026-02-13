use anyhow::Result;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use frankenterm_dynamic::Value;
use frankenterm_scripting::{
    Action, ConfigValue, EngineCapabilities, ExtensionId, ExtensionManifest, HookHandler, HookId,
    ScriptingDispatcher, ScriptingEngine,
};
use std::path::Path;
use std::sync::Arc;

#[derive(Default)]
struct NoopEngine {
    name: String,
}

impl NoopEngine {
    fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }
}

impl ScriptingEngine for NoopEngine {
    fn eval_config(&self, _path: &Path) -> Result<ConfigValue> {
        Ok(Value::Null)
    }

    fn register_hook(&self, _event: &str, _handler: HookHandler) -> Result<HookId> {
        Ok(0)
    }

    fn unregister_hook(&self, _id: HookId) -> Result<()> {
        Ok(())
    }

    fn fire_event(&self, _event: &str, _payload: &Value) -> Result<Vec<Action>> {
        Ok(Vec::new())
    }

    fn load_extension(&self, _manifest: &ExtensionManifest) -> Result<ExtensionId> {
        Ok(0)
    }

    fn unload_extension(&self, _id: ExtensionId) -> Result<()> {
        Ok(())
    }

    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities::default()
    }

    fn engine_name(&self) -> &str {
        &self.name
    }
}

fn bench_dispatch_overhead(c: &mut Criterion) {
    let payload = Value::Null;

    let engine = NoopEngine::named("noop");
    c.bench_function("dyn_engine_fire_event_no_hooks", |b| {
        b.iter(|| {
            black_box(&engine)
                .fire_event(black_box("pane-output"), black_box(&payload))
                .unwrap();
        })
    });

    let single_dispatcher =
        ScriptingDispatcher::new(vec![Arc::new(NoopEngine::named("single"))], 0).unwrap();
    c.bench_function("dispatcher_fire_event_one_engine", |b| {
        b.iter(|| {
            black_box(&single_dispatcher)
                .fire_event(black_box("pane-output"), black_box(&payload))
                .unwrap();
        })
    });

    let dual_dispatcher = ScriptingDispatcher::new(
        vec![
            Arc::new(NoopEngine::named("lua")),
            Arc::new(NoopEngine::named("wasm")),
        ],
        0,
    )
    .unwrap();
    c.bench_function("dispatcher_fire_event_two_engines", |b| {
        b.iter(|| {
            black_box(&dual_dispatcher)
                .fire_event(black_box("pane-output"), black_box(&payload))
                .unwrap();
        })
    });

    #[cfg(feature = "lua")]
    {
        use frankenterm_scripting::LuaEngine;
        use std::path::Path;
        use std::sync::atomic::{AtomicU64, Ordering};

        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/frankenterm-core/tests/fixtures/setup/wezterm_present.lua");
        let mut lua_group = c.benchmark_group("lua_engine");

        lua_group.bench_function("eval_config_fixture", |b| {
            let engine = LuaEngine::new();
            b.iter(|| {
                black_box(&engine).eval_config(black_box(&fixture)).unwrap();
            });
        });

        lua_group.bench_function("register_hook_throughput", |b| {
            let engine = LuaEngine::new();
            let counter = AtomicU64::new(0);
            b.iter(|| {
                let id = counter.fetch_add(1, Ordering::Relaxed);
                black_box(&engine)
                    .register_hook(
                        "bench-event",
                        HookHandler::new(
                            0,
                            Some("bench-event".to_string()),
                            move |_event, _payload| {
                                Ok(vec![Action::Custom {
                                    name: "id".to_string(),
                                    payload: Value::U64(id),
                                }])
                            },
                        ),
                    )
                    .unwrap();
            });
        });

        lua_group.bench_function("fire_event_roundtrip_rust_hook", |b| {
            let engine = LuaEngine::new();
            engine
                .register_hook(
                    "bench-event",
                    HookHandler::new(0, Some("bench-event".to_string()), |_event, _payload| {
                        Ok(vec![Action::Custom {
                            name: "ok".to_string(),
                            payload: Value::Bool(true),
                        }])
                    }),
                )
                .unwrap();

            b.iter(|| {
                black_box(&engine)
                    .fire_event(black_box("bench-event"), black_box(&Value::Null))
                    .unwrap();
            });
        });

        lua_group.finish();
    }
}

criterion_group!(benches, bench_dispatch_overhead);
criterion_main!(benches);

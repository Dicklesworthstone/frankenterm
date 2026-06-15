use config::{merge_dynamic_overrides, parse_toml_config_from_str};
use criterion::{criterion_group, criterion_main, Criterion};
use frankenterm_dynamic::{ToDynamic, Value};
use std::collections::BTreeMap;
use std::hint::black_box;

fn object(entries: &[(&str, Value)]) -> Value {
    Value::Object(
        entries
            .iter()
            .map(|(key, value)| (Value::String((*key).to_string()), value.clone()))
            .collect::<BTreeMap<_, _>>()
            .into(),
    )
}

fn small_config() -> String {
    r#"
scrollback_lines = 12000
scrollback_tiered_enabled = true
scrollback_hot_lines = 1500
scrollback_warm_max_mb = 48
font_size = 14.5
color_scheme = "Builtin Dark"
enable_scroll_bar = true
enable_tab_bar = true
initial_rows = 40
initial_cols = 120
"#
    .to_string()
}

fn medium_config() -> String {
    let mut config = small_config();
    for idx in 0..20 {
        config.push_str(&format!(
            r#"
[[ssh_domains]]
name = "cluster-{idx}"
remote_address = "10.0.0.{idx}:22"
username = "agent"
connect_automatically = true
"#
        ));
    }
    config
}

fn large_config() -> String {
    let mut config = medium_config();
    for idx in 0..120 {
        config.push_str(&format!(
            r#"
[[ssh_domains]]
name = "fleet-{idx}"
remote_address = "swarm-{idx}.example.com:22"
username = "operator"
no_agent_auth = true
"#
        ));
    }
    config
}

fn bench_config_loading(c: &mut Criterion) {
    let overrides = Value::default();
    let small = small_config();
    let medium = medium_config();
    let large = large_config();

    let mut group = c.benchmark_group("config_loading");
    group.bench_function("small", |b| {
        b.iter(|| {
            parse_toml_config_from_str(black_box(&small), black_box(&overrides))
                .expect("small config should parse")
        })
    });
    group.bench_function("medium", |b| {
        b.iter(|| {
            parse_toml_config_from_str(black_box(&medium), black_box(&overrides))
                .expect("medium config should parse")
        })
    });
    group.bench_function("large", |b| {
        b.iter(|| {
            parse_toml_config_from_str(black_box(&large), black_box(&overrides))
                .expect("large config should parse")
        })
    });
    group.finish();
}

fn bench_override_merge(c: &mut Criterion) {
    let base = object(&[
        ("scrollback_lines", 5000usize.to_dynamic()),
        ("font_size", 13.0f64.to_dynamic()),
        ("enable_scroll_bar", false.to_dynamic()),
    ]);
    let file = object(&[
        ("scrollback_lines", 10000usize.to_dynamic()),
        ("initial_rows", 40usize.to_dynamic()),
        ("initial_cols", 120usize.to_dynamic()),
    ]);
    let env = object(&[
        ("font_size", 15.0f64.to_dynamic()),
        ("enable_scroll_bar", true.to_dynamic()),
    ]);
    let cli = object(&[
        ("scrollback_lines", 15000usize.to_dynamic()),
        ("color_scheme", "Builtin Solarized Dark".to_dynamic()),
    ]);

    c.bench_function("config_layer_merge", |b| {
        b.iter(|| {
            let mut merged = black_box(base.clone());
            merge_dynamic_overrides(&mut merged, black_box(&file));
            merge_dynamic_overrides(&mut merged, black_box(&env));
            merge_dynamic_overrides(&mut merged, black_box(&cli));
            black_box(merged)
        })
    });
}

criterion_group!(benches, bench_config_loading, bench_override_merge);
criterion_main!(benches);

//! Regression coverage for br-ft-3twzm / ft-l1jgo read-pool migration.
//!
//! The br-ft-3twzm fix migrates 9 reader paths in storage.rs to
//! `pooled_backend`, restoring the connection-pool contract ft-bhyxz
//! introduced. Direct in-process verification
//! that "the pool is hit, not RusqliteBackend::open" requires
//! either:
//!   - Instrumenting `rusqlite::Connection::open` (not exposed to
//!     downstream crates).
//!   - Wrapping `PooledReadConn::acquire` in a counter (substrate
//!     change beyond the scope of this fix).
//!
//! The migrated reader paths are exercised by their existing unit
//! / integration tests:
//!   - `embedding_stats`, `get_embedding`, `get_unembedded_segments`,
//!     `store_embedding` — covered by `tests/proptest_storage*.rs`
//!     + the storage handle's own #[cfg(test)] module.
//!   - `get_saved_search_by_name`, `list_saved_searches` — covered
//!     by storage's saved-search test cluster.
//!   - `get_gaps`, `retention_cleanup_count`, `segment_time_range` —
//!     covered by storage's reader-path tests.
//!
//! The structural verification below keeps the ft-l1jgo acceptance
//! invariant pinned across `storage.rs` and its extracted handle files.
//! It checks source structure, not runtime pool utilization. Only items
//! explicitly gated by `cfg(test)` are excluded; other configurations
//! remain checked regardless of this test runner's enabled features.

use std::fs;
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Path>()
                .is_ok_and(|path| path.is_ident("test"))
    })
}

#[derive(Default)]
struct ConnectionReferences {
    count: usize,
}

impl ConnectionReferences {
    fn tokens(&mut self, tokens: proc_macro2::TokenStream) {
        for token in tokens {
            match token {
                proc_macro2::TokenTree::Ident(ident) => self.visit_ident(&ident),
                proc_macro2::TokenTree::Group(group) => self.tokens(group.stream()),
                // Comments and string contents are not Rust identifiers.
                proc_macro2::TokenTree::Literal(_) | proc_macro2::TokenTree::Punct(_) => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for ConnectionReferences {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let attrs: &[syn::Attribute] = match item {
            syn::Item::Const(item) => &item.attrs,
            syn::Item::Enum(item) => &item.attrs,
            syn::Item::ExternCrate(item) => &item.attrs,
            syn::Item::Fn(item) => &item.attrs,
            syn::Item::ForeignMod(item) => &item.attrs,
            syn::Item::Impl(item) => &item.attrs,
            syn::Item::Macro(item) => &item.attrs,
            syn::Item::Mod(item) => &item.attrs,
            syn::Item::Static(item) => &item.attrs,
            syn::Item::Struct(item) => &item.attrs,
            syn::Item::Trait(item) => &item.attrs,
            syn::Item::TraitAlias(item) => &item.attrs,
            syn::Item::Type(item) => &item.attrs,
            syn::Item::Union(item) => &item.attrs,
            syn::Item::Use(item) => &item.attrs,
            // Unknown/verbatim items are scanned, never silently excluded.
            _ => &[],
        };
        if !test_only(attrs) {
            visit::visit_item(self, item);
        }
    }

    fn visit_ident(&mut self, ident: &'ast syn::Ident) {
        if ident.to_string().trim_start_matches("r#") == "Connection" {
            self.count += 1;
        }
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let attrs: &[syn::Attribute] = match item {
            syn::ImplItem::Const(item) => &item.attrs,
            syn::ImplItem::Fn(item) => &item.attrs,
            syn::ImplItem::Type(item) => &item.attrs,
            syn::ImplItem::Macro(item) => &item.attrs,
            _ => &[],
        };
        if !test_only(attrs) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        let attrs: &[syn::Attribute] = match item {
            syn::TraitItem::Const(item) => &item.attrs,
            syn::TraitItem::Fn(item) => &item.attrs,
            syn::TraitItem::Type(item) => &item.attrs,
            syn::TraitItem::Macro(item) => &item.attrs,
            _ => &[],
        };
        if !test_only(attrs) {
            visit::visit_trait_item(self, item);
        }
    }

    fn visit_foreign_item(&mut self, item: &'ast syn::ForeignItem) {
        let attrs: &[syn::Attribute] = match item {
            syn::ForeignItem::Fn(item) => &item.attrs,
            syn::ForeignItem::Static(item) => &item.attrs,
            syn::ForeignItem::Type(item) => &item.attrs,
            syn::ForeignItem::Macro(item) => &item.attrs,
            _ => &[],
        };
        if !test_only(attrs) {
            visit::visit_foreign_item(self, item);
        }
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        visit::visit_path(self, &mac.path);
        self.tokens(mac.tokens.clone());
    }

    fn visit_token_stream(&mut self, tokens: &'ast proc_macro2::TokenStream) {
        self.tokens(tokens.clone());
    }
}

fn connection_references(source: &str) -> syn::Result<usize> {
    let file = syn::parse_file(source)?;
    let mut references = ConnectionReferences::default();
    if !test_only(&file.attrs) {
        references.visit_file(&file);
    }
    Ok(references.count)
}

fn collect_handle_sources(directory: &Path, sources: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("read storage handle directory") {
        let path = entry.expect("read storage handle entry").path();
        if path.is_dir() {
            collect_handle_sources(&path, sources);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(path);
        }
    }
}

#[test]
fn storage_rs_keeps_direct_connection_refs_out_of_handle_surface() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = vec![root.join("src/storage.rs")];
    collect_handle_sources(&root.join("src/storage/handle"), &mut sources);
    assert!(sources.contains(&root.join("src/storage/handle/mod.rs")));
    assert!(sources.contains(&root.join("src/storage/handle/event_mutes.rs")));
    sources.sort();
    for path in sources {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        let count = connection_references(&source)
            .unwrap_or_else(|err| panic!("parse {}: {err}", path.display()));
        assert_eq!(
            count,
            0,
            "{} must route direct Connection access through StorageBackend",
            path.display()
        );
    }
}

#[test]
fn production_connections_are_detected_despite_formatting_aliases_and_macros() {
    for source in [
        "fn f() { rusqlite :: Connection :: open(\"db\"); }",
        "use rusqlite::{params, Connection as Db};",
        "fn f() { Connection::open_in_memory(); }",
        "fn f(_: &Connection, _: &mut Connection) {}",
        "fn f<T: Deref<Target = Connection>>() {}",
        "macro_rules! open { () => { rusqlite::Connection::open(\"db\") }; }",
        "fn f() { backend!(rusqlite::Connection); }",
        "#[cfg(any(test, feature = \"live\"))] use rusqlite::Connection;",
        "#[cfg(not(test))] use rusqlite::Connection;",
        "use rusqlite::r#Connection;",
    ] {
        assert!(
            connection_references(source).expect("valid planted Rust") > 0,
            "missed production reference: {source}"
        );
    }
}

#[test]
fn test_items_comments_and_strings_do_not_hide_later_production() {
    let allowed = r####"
        #[cfg(test)] use rusqlite::Connection;
        #[cfg(test)] mod fixtures {
            const BRACES: &str = r###"{ } } { café"###;
            fn f() { rusqlite::Connection::open_in_memory(); }
        }
        /// Connection::open is forbidden in production.
        fn allowed() { let _ = "rusqlite::Connection { }"; }
    "####;
    assert_eq!(connection_references(allowed).unwrap(), 0);
    let planted = format!("{allowed}\nfn production(_: &Connection) {{}}");
    assert_eq!(connection_references(&planted).unwrap(), 1);
    assert_eq!(
        connection_references("#![cfg(test)] use rusqlite::Connection;").unwrap(),
        0
    );
}

#[test]
fn malformed_rust_is_an_error_not_a_clean_scan() {
    assert!(connection_references("fn broken( {").is_err());
}

#[test]
fn associated_test_items_do_not_hide_neighboring_production_items() {
    for (allowed, production) in [
        (
            "impl S { #[cfg(test)] fn fixture(_: &Connection) {} }",
            "impl S { #[cfg(test)] fn fixture(_: &Connection) {} fn live(_: &Connection) {} }",
        ),
        (
            "trait T { #[cfg(test)] fn fixture(_: &Connection); }",
            "trait T { #[cfg(test)] fn fixture(_: &Connection); fn live(_: &Connection); }",
        ),
        (
            "unsafe extern \"C\" { #[cfg(test)] fn fixture(_: *mut Connection); }",
            "unsafe extern \"C\" { #[cfg(test)] fn fixture(_: *mut Connection); fn live(_: *mut Connection); }",
        ),
    ] {
        assert_eq!(connection_references(allowed).unwrap(), 0);
        assert_eq!(connection_references(production).unwrap(), 1);
    }
}

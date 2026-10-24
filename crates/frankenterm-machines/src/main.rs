//! `frankenterm-machines`, bundled inside the Mac companion (FrankenTerm
//! Export). At launch the companion runs `frankenterm-machines source` to load
//! this Mac's FrankenTerm machines and the private keys their routes use,
//! without the user choosing files. It prints the same JSON as
//! `ft mobile source` (see `config::mobile_export::companion_source`).

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("source")) {
        anyhow::bail!("usage: frankenterm-machines source [--inventory <machines.toml>]");
    }
    let mut inventory = None;
    while let Some(arg) = args.next() {
        if arg == "--inventory" {
            let path = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("--inventory needs a path"))?;
            inventory = Some(PathBuf::from(path));
        } else {
            anyhow::bail!("unknown argument {}", arg.to_string_lossy());
        }
    }
    let path = inventory.unwrap_or_else(config::machines::default_path);
    let source = config::mobile_export::load_companion_source(&path)?;
    println!("{}", serde_json::to_string(&source)?);
    Ok(())
}

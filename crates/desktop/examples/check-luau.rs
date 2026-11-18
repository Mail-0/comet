use anyhow::{Result, ensure};
use zgx_script::check;

fn main() -> Result<()> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let report = check::check(&[path], &check::Options::default())?;
    for diagnostic in &report.diagnostics {
        println!("{diagnostic}");
    }
    println!(
        "Luau strict: {} files, {} errors, {} warnings",
        report.files,
        report.errors(),
        report.warnings()
    );
    ensure!(report.diagnostics.is_empty(), "Luau strict check failed");
    Ok(())
}

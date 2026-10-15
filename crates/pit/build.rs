// The dashboard is embedded from packages/web/dist. Make sure the folder
// exists so the crate builds before the dashboard has been built once.
fn main() {
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/web/dist");
    if !dist.exists() {
        let _ = std::fs::create_dir_all(&dist);
    }
    println!("cargo:rerun-if-changed=../../packages/web/dist");
}

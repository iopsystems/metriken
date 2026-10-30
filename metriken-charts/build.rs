// include_dir! tracks only the files that existed when it ran, so a file
// added under assets/ would not rebuild the crate. Cargo compares the
// mtimes under a directory named here recursively.
fn main() {
    println!("cargo:rerun-if-changed=assets");
}

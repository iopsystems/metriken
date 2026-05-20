// Windows: link `rstrtmgr.lib` (the Restart Manager API). libduckdb-sys's
// bundled DuckDB sources call `RmStartSession` / `RmEndSession` /
// `RmRegisterResources` / `RmGetList` from `duckdb::AdditionalLockInfo`
// to surface nicer "this file is held by ..." errors on locked files,
// but doesn't add the corresponding link directive to its build script.
// Without this, the rezolus/metriken workspace fails to link on
// x86_64-pc-windows-msvc with LNK2019 unresolved external symbol errors.
//
// Filed upstream as a downstream workaround; remove this build.rs once
// duckdb-rs#... is released. This is a no-op on every other platform.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-lib=dylib=rstrtmgr");
    }
}

//! Build script: the kill guard shim (`shim/killguard.c`, see `src/killguard.rs`), compiled into a
//! shared object in `OUT_DIR` that `src/killguard.rs` embeds with `include_bytes!`. Linux targets
//! with glibc only (the shim needs libc.so.6); on every other target, musl included, this does
//! nothing and the guard is compiled out.
//!
//! Only `build.rs` and the `.c` file are watched, so a commit doesn't rerun it.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=shim/killguard.c");
    let target = |key: &str| std::env::var(key).unwrap_or_default();
    if target("CARGO_CFG_TARGET_OS") != "linux" || target("CARGO_CFG_TARGET_ENV") != "gnu" {
        return;
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let so = out.join("synaps-killguard.so");
    let source = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"))
        .join("shim")
        .join("killguard.c");
    // The target's C compiler, as the `cc` crate finds it (CC, CC_<target>, cross toolchains).
    let compiler = cc::Build::new()
        .cargo_metadata(false)
        .try_get_compiler()
        .unwrap_or_else(|e| panic!("kill guard shim: no C compiler for the target: {e}"));
    // Its path only, with these flags and none of what cc gathers (CFLAGS, CC_<target> args,
    // defaults): a -fsanitize= or -m32 from the env would give the shim a runtime or an ELF class
    // a tool child may not have, and ld.so then fails every exec.
    let status = Command::new(compiler.path())
        .args(["-shared", "-fPIC", "-O2", "-Wall", "-o"])
        .arg(&so)
        .arg(&source)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "kill guard shim: cannot run the C compiler {}: {e}",
                compiler.path().display()
            )
        });
    assert!(
        status.success(),
        "kill guard shim: {} failed ({status}) building shim/killguard.c",
        compiler.path().display()
    );
}

//! Compile cxxdep.cpp with the environment's C++ driver against libc++, archive it, and link it statically
//! plus libc++ and libc++abi dynamically.
use std::{env, path::PathBuf, process::Command};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let cxx = env::var("CXX").unwrap_or_else(|_| "c++".into());
    let include = env::var("PROBE_LIBCXX_INCLUDE").expect("PROBE_LIBCXX_INCLUDE names libc++'s c++/v1 headers");
    let object = out.join("cxxdep.o");
    let status = Command::new(&cxx)
        .args(["-nostdinc++", "-isystem", &include, "-fPIC", "-c", "cxxdep.cpp", "-o"])
        .arg(&object)
        .status()
        .expect("run the C++ driver");
    assert!(status.success(), "compiling cxxdep.cpp failed");
    let status = Command::new(env::var("AR").unwrap_or_else(|_| "ar".into()))
        .arg("crs")
        .arg(out.join("libprobecxx.a"))
        .arg(&object)
        .status()
        .expect("run ar");
    assert!(status.success(), "archiving cxxdep.o failed");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=probecxx");
    println!("cargo:rustc-link-lib=dylib=c++");
    println!("cargo:rustc-link-lib=dylib=c++abi");
    println!("cargo:rerun-if-changed=cxxdep.cpp");
    println!("cargo:rerun-if-env-changed=PROBE_LIBCXX_INCLUDE");
}

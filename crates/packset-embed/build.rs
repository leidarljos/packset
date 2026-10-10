//! Fail in seconds, not at the link, when no ONNX Runtime is in reach.
//!
//! The default build links an ONNX Runtime that is already on the machine.
//! Without one, `cargo install packset-embed` used to compile for minutes
//! and then stop at the linker with an error that never named the runtime.
//! This script runs as soon as the build starts, while the dependencies
//! compile, and stops the build with the recipe instead. It checks the same places `ort` looks, and no
//! more: a feature that brings or loads its own runtime, `ORT_LIB_PATH`
//! (or `ORT_LIB_LOCATION`), or a `libonnxruntime` of 1.24 or newer that
//! pkg-config can see. `PACKSET_EMBED_NO_ORT_CHECK=1` skips the check.

use std::process::Command;

const MIN_ORT: &str = "1.24";

fn set(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|v| !v.is_empty())
}

fn pkg_config_has_ort() -> bool {
    if set("LIBONNXRUNTIME_NO_PKG_CONFIG") {
        return false;
    }
    let pkg_config = std::env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".into());
    Command::new(pkg_config)
        .args([&format!("--atleast-version={MIN_ORT}"), "libonnxruntime"])
        .status()
        .is_ok_and(|s| s.success())
}

fn main() {
    for key in [
        "ORT_LIB_PATH",
        "ORT_LIB_LOCATION",
        "PKG_CONFIG",
        "PKG_CONFIG_PATH",
        "LIBONNXRUNTIME_NO_PKG_CONFIG",
        "PACKSET_EMBED_NO_ORT_CHECK",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    let found = set("CARGO_FEATURE_DOWNLOAD_BINARIES")
        || set("CARGO_FEATURE_LOAD_DYNAMIC")
        || set("ORT_LIB_PATH")
        || set("ORT_LIB_LOCATION")
        || set("DOCS_RS")
        || set("PACKSET_EMBED_NO_ORT_CHECK")
        || pkg_config_has_ort();
    if found {
        return;
    }
    let script = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default())
        .join("build-onnxruntime.sh");
    panic!(
        "\n\npackset-embed links an ONNX Runtime that is already on this machine, and none was found:\n\
         no ORT_LIB_PATH, and pkg-config has no libonnxruntime of {MIN_ORT} or newer.\n\n\
         Build one (1.28.0, the version ort publishes) and point the build at it:\n\n\
         \x20   bash {script} \"$HOME/onnxruntime\"\n\
         \x20   ORT_LIB_PATH=$HOME/onnxruntime/lib ORT_PREFER_DYNAMIC_LINK=1 cargo install --locked packset-embed\n\
         \x20   export LD_LIBRARY_PATH=$HOME/onnxruntime/lib\n\n\
         Or take the release tarball with `cargo binstall packset-embed`, or the prebuilt runtime\n\
         with `--features download-binaries`. The howto has the details.\n",
        script = script.display()
    );
}

use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let core = PathBuf::from("vendor/tone3000-plugin/plugin/NeuralAmpModelerCore");
    let nam = core.join("NAM");
    if !nam.join("get_dsp.cpp").is_file() {
        panic!(
            "NeuralAmpModelerCore is missing. Follow README.md and clone tone3000-plugin with its NeuralAmpModelerCore submodule into vendor/tone3000-plugin"
        );
    }

    // NeuralAmpModelerCore includes both as headers. pkg-config keeps the
    // system dependency contract explicit instead of reaching into JUCE or the
    // TONE3000 wrapper. The current upstream core also requires Eigen.
    let nlohmann = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("nlohmann_json")
        .expect("nlohmann_json not found; install nlohmann-json3-dev");
    let eigen = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("eigen3")
        .expect("eigen3 not found; install libeigen3-dev");

    let sources = [
        "activations.cpp",
        "container.cpp",
        "conv1d.cpp",
        "convnet.cpp",
        "dsp.cpp",
        "get_dsp.cpp",
        "linear.cpp",
        "lstm.cpp",
        "ring_buffer.cpp",
        "util.cpp",
        "wavenet/a2_fast.cpp",
        "wavenet/model.cpp",
        "wavenet/slimmable.cpp",
    ];

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .flag("-include")
        .flag("src/eigen_compat.h")
        .define("NAM_SAMPLE_FLOAT", None)
        .define("NAM_ENABLE_A2_FAST", None)
        .define("NAM_DEFAULT_MAX_BUFFER_SIZE", Some("256"))
        .include(&core)
        .include(&nam)
        .include(core.join("Dependencies/nlohmann"))
        .file("src/nam_bridge.cpp")
        .cargo_metadata(false)
        .warnings(false);

    for include in nlohmann.include_paths.iter().chain(&eigen.include_paths) {
        build.include(include);
    }
    for source in sources {
        build.file(nam.join(source));
    }
    build.compile("nam_core_ffi");

    // Parser registration is performed by static initializers in otherwise
    // unreferenced core object files. Keep the whole core archive so WaveNet,
    // LSTM, ConvNet, Linear, container, and slimmable NAM files all register.
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo provides OUT_DIR"));
    println!("cargo:rustc-link-search=native={}", output.display());
    println!("cargo:rustc-link-lib=static:+whole-archive=nam_core_ffi");
    println!("cargo:rustc-link-lib=dylib=stdc++");
    println!("cargo:rustc-link-lib=static=stdc++fs");

    println!("cargo:rerun-if-changed=src/nam_bridge.cpp");
    println!("cargo:rerun-if-changed=src/eigen_compat.h");
    for source in sources {
        rerun_if_changed(&nam.join(source));
    }
    rerun_if_changed(&nam.join("get_dsp.h"));
    rerun_if_changed(&nam.join("dsp.h"));
}

fn rerun_if_changed(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
}

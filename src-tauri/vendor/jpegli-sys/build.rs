use std::{
    env,
    path::{Path, PathBuf},
};

fn source_dir() -> PathBuf {
    env::var("DEP_JXL_PATH").map_or_else(
        |_| Path::new(env!("CARGO_MANIFEST_DIR")).join("libjxl"),
        PathBuf::from,
    )
}

fn main() {
    let source = source_dir();

    if let Ok(p) = std::thread::available_parallelism() {
        env::set_var("CMAKE_BUILD_PARALLEL_LEVEL", format!("{}", p));
    }

    // 始终 Release+/MD，避免 MSVC Debug CRT（MSVCRTD）与 Rust 默认 CRT 冲突（LNK4098/_CrtDbgReport）。
    let mut config = cmake::Config::new(&source);
    config
        .profile("Release")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("BUILD_TESTING", "OFF")
        .define("JPEGXL_ENABLE_JPEGLI", "ON")
        .define("JPEGXL_ENABLE_TOOLS", "OFF")
        .define("JPEGXL_ENABLE_DOXYGEN", "OFF")
        .define("JPEGXL_ENABLE_MANPAGES", "OFF")
        .define("JPEGXL_ENABLE_BENCHMARK", "OFF")
        .define("JPEGXL_ENABLE_EXAMPLES", "OFF")
        .define("JPEGXL_ENABLE_JNI", "OFF")
        .define("JPEGXL_ENABLE_SJPEG", "OFF")
        .define("JPEGXL_ENABLE_OPENEXR", "OFF")
        .define("JPEGLI_LIBJPEG_LIBRARY_SOVERSION", "8")
        .define("JPEGLI_LIBJPEG_LIBRARY_VERSION", "8.2.2")
        .build_target("jpegli-static");

    let dst = config.build();
    let build = dst.join("build");

    let lib_candidates = [
        build.join("lib").join("Release"),
        build.join("lib"),
        build.join("lib").join("Debug"),
    ];
    let hwy_candidates = [
        build.join("third_party").join("highway").join("Release"),
        build.join("third_party").join("highway"),
        build.join("third_party").join("highway").join("Debug"),
    ];

    let mut found_lib = false;
    for p in &lib_candidates {
        if p.join("jpegli-static.lib").exists() || p.join("libjpegli-static.a").exists() {
            println!("cargo:rustc-link-search=native={}", p.display());
            found_lib = true;
            break;
        }
    }
    if !found_lib {
        println!("cargo:rustc-link-search=native={}", build.join("lib").display());
    }

    let mut found_hwy = false;
    for p in &hwy_candidates {
        if p.join("hwy.lib").exists() || p.join("libhwy.a").exists() {
            println!("cargo:rustc-link-search=native={}", p.display());
            found_hwy = true;
            break;
        }
    }
    if !found_hwy {
        println!(
            "cargo:rustc-link-search=native={}",
            build.join("third_party").join("highway").display()
        );
    }

    println!("cargo:rustc-link-lib=static=jpegli-static");
    println!("cargo:rustc-link-lib=static=hwy");

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    println!("cargo:rustc-link-lib=c++");
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_env = "msvc"
    )))]
    println!("cargo:rustc-link-lib=stdc++");
}

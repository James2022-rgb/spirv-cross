#![allow(clippy::needless_return)]

use bindgen::EnumVariation;
use flate2::read::GzDecoder;
use std::{
    env,
    path::{Path, PathBuf},
};
use tar::Archive;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let out_path = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH")?;
    let sysroot = match target_arch.as_str() {
        "wasm" | "wasm32" | "wasm64" => Some(wasi_sdk(&out_path).await?),
        _ => None,
    };

    let lib_sysroot = sysroot.clone();
    let lib = tokio::task::spawn_blocking(move || build_library(lib_sysroot));
    let bindings = tokio::task::spawn_blocking(move || generate_bindings(out_path, sysroot));

    let (lib, _) = tokio::try_join!(lib, bindings)?;
    lib?;

    return Ok(());
}

fn build_library(wasi_sdk: Option<PathBuf>) -> anyhow::Result<()> {
    let mut build = cmake::Config::new("SPIRV-Cross");
    build
        .define("SPIRV_CROSS_CLI", "OFF")
        .define("SPIRV_CROSS_ENABLE_TESTS", "OFF")
        .define("SPIRV_CROSS_ENABLE_C_API", "ON")
        .define("SPIRV_CROSS_SKIP_INSTALL", "ON")
        .define("SPIRV_CROSS_STATIC", "ON")
        .define("SPIRV_CROSS_SHARED", "OFF");

    if let Some(ref sdk) = wasi_sdk {
        build
            .define("SPIRV_CROSS_EXCEPTIONS_TO_ASSERTIONS", "ON")
            .define(
                "CMAKE_TOOLCHAIN_FILE",
                sdk.join("share/cmake/wasi-sdk.cmake"),
            )
            .define("WASI_SDK_PREFIX", sdk)
            .cxxflag(format!(
                "-L{}",
                sdk.join("share/wasi-sysroot/lib/wasm32-wasi").display()
            ))
            .cxxflag("-lc")
            .cxxflag("-lc++")
            .cxxflag("-lc++abi")
            .cxxflag("-lc++experimental");
    }

    build.define(
        "SPIRV_CROSS_ENABLE_GLSL",
        cmake_flag(cfg!(feature = "glsl")),
    );

    build.define(
        "SPIRV_CROSS_ENABLE_HLSL",
        cmake_flag(cfg!(feature = "hlsl")),
    );

    build.define("SPIRV_CROSS_ENABLE_MSL", cmake_flag(cfg!(feature = "msl")));

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();

    if target_os == "android" {
        // We need to figure out the values for `CMAKE_MAKE_PROGRAM` and `CMAKE_TOOLCHAIN_FILE` if we aim to support Building for Android on other platforms.
        assert!(cfg!(target_os = "windows"), "Building for Android is currently only supported for Windows.");

        let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();

        let android_ndk_home = env::var("ANDROID_NDK_HOME")
            .expect("Environment variable ANDROID_NDK_HOME not set !");
        let android_abi_name = match target_arch.as_str() {
            "aarch64" => "arm64-v8a",
            "arm"     => "armeabi-v7a",
            _ => panic!("Unexpected CARGO_CFG_TARGET_ARCH: {:?}", target_arch),
        };

        build.generator("Unix Makefiles");
        build.define("CMAKE_BUILD_TYPE", "Release");
        if let Some(cpp) = cpp_stdlib(&target_os) {
            build.define("ANDROID_STL", cpp);
        }
        build.define("ANDROID_ABI", android_abi_name);
        build.define("ANDROID_PLATFORM", "android-24");
        build.define("CMAKE_SYSTEM_NAME", "Android");
        build.define("ANDROID_TOOLCHAIN", "clang");
        build.define("ANDROID_ARM_MODE", "arm");
        build.define("CMAKE_MAKE_PROGRAM", format!(r#"{}/prebuilt/windows-x86_64/bin/make.exe"#, android_ndk_home));
        build.define("CMAKE_TOOLCHAIN_FILE", format!(r#"{}/build/cmake/android.toolchain.cmake"#, android_ndk_home));

        // So `cmake` doesn't set `--target=aarch64-linux-android` for these:
        build.define("CMAKE_C_FLAGS", "");
        build.define("CMAKE_CXX_FLAGS", "");
    }

    let out_path = build.no_build_target(true).build().join("build");
    let out_path = if target_os == "windows" {
        out_path.join(build.get_profile())
    }
    else {
        out_path
    };

    println!("cargo:rustc-link-search=native={}", out_path.display());
    let ext = match (target_os == "windows").then(|| build.get_profile()) {
        Some("Debug") => "d",
        _ => "",
    };

    for entry in [
        "spirv-cross-c",
        "spirv-cross-core",
        "spirv-cross-cpp",
        "spirv-cross-reflect",
        "spirv-cross-util",
    ] {
        println!("cargo:rustc-link-lib=static={entry}{ext}",);
    }

    #[cfg(feature = "glsl")]
    println!("cargo:rustc-link-lib=static=spirv-cross-glsl{ext}");
    #[cfg(feature = "hlsl")]
    println!("cargo:rustc-link-lib=static=spirv-cross-hlsl{ext}");
    #[cfg(feature = "msl")]
    println!("cargo:rustc-link-lib=static=spirv-cross-msl{ext}");

    if let Some(sysroot) = wasi_sdk {
        println!(
            "cargo:rustc-link-search=native={}",
            sysroot.join("share/wasi-sysroot/lib/wasm32-wasi").display()
        );

        println!("cargo:rustc-link-lib=static=c");
        println!("cargo:rustc-link-lib=static=c++");
        println!("cargo:rustc-link-lib=static=c++abi");
        println!("cargo:rustc-link-lib=static=c++experimental");
    } else if let Some(cpp) = cpp_stdlib(&std::env::var("TARGET")?) {
        println!("cargo:rustc-link-lib={cpp}");
    }

    return Ok(());
}

fn generate_bindings(out_path: PathBuf, wasi_sysroot: Option<PathBuf>) {
    let mut builder = bindgen::Builder::default()
        .header("SPIRV-Cross/spirv_cross_c.h")
        .clang_arg("-DSPVC_EXPORT_SYMBOLS");

    if let Some(sysroot) = wasi_sysroot {
        builder = builder.clang_arg(format!("--sysroot={}", sysroot.display()));
    }

    // For native targets, include all types and functions
    builder
        .default_enum_style(EnumVariation::Rust {
            non_exhaustive: true,
        })
        .layout_tests(false)
        .generate()
        .expect("Unable to generate bindings")
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}

async fn wasi_sdk(out_path: &Path) -> anyhow::Result<PathBuf> {
    let latest_release = octocrab::instance()
        .repos("WebAssembly", "wasi-sdk")
        .releases()
        .get_latest()
        .await?;

    let target = 'brk: {
        cfg_if::cfg_if! {
            if #[cfg(windows)] {
                break 'brk ".m-mingw";
            } else if #[cfg(target_os = "linux")] {
                break 'brk "-linux";
            } else if #[cfg(target_os = "macos")] {
                break 'brk "-macos";
            }
        }
    };

    let full_version = format!("{}.0", latest_release.name.unwrap());
    let asset_name = format!("{full_version}{target}.tar.gz");

    for asset in latest_release.assets {
        if asset.name == asset_name {
            if !tokio::fs::try_exists(out_path.join(&full_version)).await? {
                let contents = reqwest::get(asset.browser_download_url)
                    .await?
                    .bytes()
                    .await?;

                let unpack_out_path = out_path.to_path_buf();
                tokio::task::spawn_blocking(move || {
                    let tar = GzDecoder::new(&contents as &[u8]);
                    let mut archive = Archive::new(tar);
                    archive.unpack(unpack_out_path)
                })
                .await??;
            }

            return Ok(out_path.join(full_version));
        }
    }

    panic!("Release not found!");
}

fn cmake_flag(v: bool) -> &'static str {
    match v {
        true => "ON",
        false => "OFF",
    }
}

fn cpp_stdlib(target: &str) -> Option<&'static str> {
    if target.contains("msvc") {
        None
    } else if target.contains("apple") || target.contains("freebsd") || target.contains("openbsd") {
        Some("c++")
    } else if target.contains("android") {
        Some("c++_shared")
    } else {
        Some("stdc++")
    }
}

extern crate bindgen;
extern crate pkg_config;

use std::env;
use std::ffi::OsString;
use std::iter;
use std::path::PathBuf;
use std::process::Command;

const FUSE_DEFAULT_API_VERSION: u32 = 35;

fn clang_resource_include_path() -> Option<PathBuf> {
    let clang = env::var_os("CLANG_PATH").unwrap_or_else(|| "clang".into());
    let output = Command::new(clang)
        .arg("-print-resource-dir")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let resource_dir = String::from_utf8(output.stdout).ok()?;
    let include_path = PathBuf::from(resource_dir.trim()).join("include");
    include_path
        .join("stdarg.h")
        .is_file()
        .then_some(include_path)
}

fn compiler_include_path(compiler: OsString) -> Option<PathBuf> {
    let output = Command::new(compiler)
        .arg("-print-file-name=include")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let include_path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    include_path
        .join("stdarg.h")
        .is_file()
        .then_some(include_path)
}

fn builtin_header_include_path() -> Option<PathBuf> {
    clang_resource_include_path().or_else(|| {
        env::var_os("CC")
            .into_iter()
            .chain([OsString::from("cc"), OsString::from("gcc")])
            .find_map(compiler_include_path)
    })
}

macro_rules! version {
    ($version_var:ident, $feature:literal, $version:literal) => {
        #[cfg(feature = $feature)]
        {
            if $version_var.is_some() {
                panic!("More than one FUSE API version feature is enabled");
            }
            $version_var = Some($version);
        }
    };
}

#[cfg(any(feature = "fuse_highlevel", feature = "fuse_lowlevel"))]
fn fuse_binding_filter(builder: bindgen::Builder) -> bindgen::Builder {
    let mut builder = builder
        // Whitelist "fuse_*" symbols and blocklist everything else
        .allowlist_type("^[fF][uU][sS][eE].*")
        .allowlist_function("^[fF][uU][sS][eE].*")
        .allowlist_var("^[fF][uU][sS][eE].*")
        .blocklist_type("fuse_log_func_t")
        .blocklist_function("fuse_set_log_func");
    // TODO: properly bind fuse_log_func_t and allowlist fuse_set_log_func again

    if cfg!(target_os = "macos") {
        // osxfuse needs this type
        builder = builder.allowlist_type("setattr_x");
    }
    builder
}

#[cfg(feature = "cuse_lowlevel")]
fn cuse_binding_filter(builder: bindgen::Builder) -> bindgen::Builder {
    builder
        // Whitelist "cuse_*" symbols and blocklist everything else
        .allowlist_type("^[cC][uU][sS][eE].*")
        .allowlist_function("^[cC][uU][sS][eE].*")
        .allowlist_var("^[cC][uU][sS][eE].*")
}

fn generate_fuse_bindings(
    header: &str,
    api_version: u32,
    fuse_lib: &pkg_config::Library,
    binding_filter: fn(bindgen::Builder) -> bindgen::Builder,
) {
    // Find header file
    let mut header_path: Option<PathBuf> = None;
    for include_path in fuse_lib.include_paths.iter() {
        let test_path = include_path.join(header);
        if test_path.exists() {
            header_path = Some(test_path);
            break;
        }
    }
    let header_path = header_path
        .unwrap_or_else(|| panic!("Cannot find {}", header))
        .to_str()
        .unwrap_or_else(|| panic!("Path to {} contains invalid unicode characters", header))
        .to_string();

    // Gather fuse defines
    let defines = fuse_lib.defines.iter().map(|(key, val)| match val {
        Some(val) => format!("-D{}={}", key, val),
        None => format!("-D{}", key),
    });
    // Gather include paths
    let includes = fuse_lib
        .include_paths
        .iter()
        .map(|dir| format!("-I{}", dir.display()));
    // API version definition
    let api_define = iter::once(format!("-DFUSE_USE_VERSION={}", api_version));
    // Compiler-provided headers such as stdarg.h live in a Clang or GCC
    // internal include directory, not /usr/include. Some libclang
    // installations fail to add this directory to their default search path.
    let builtin_header_include = builtin_header_include_path()
        .into_iter()
        .flat_map(|dir| ["-isystem".to_string(), dir.display().to_string()]);
    // Chain compile flags
    let compile_flags = defines
        .chain(includes)
        .chain(api_define)
        .chain(builtin_header_include);

    // Create bindgen builder
    let mut builder = bindgen::builder()
        // Add clang flags
        .clang_args(compile_flags)
        // Derive Debug, Copy and Default
        .derive_default(true)
        .derive_copy(true)
        .derive_debug(true)
        // Add CargoCallbacks so build.rs is rerun on header changes
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    builder = binding_filter(builder);

    // Generate bindings
    let bindings = builder
        .header(header_path)
        .generate()
        .unwrap_or_else(|_| panic!("Failed to generate {} bindings", header));

    // Write bindings to file
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bindings_path = out_dir.join(header.replace(".h", ".rs"));
    bindings
        .write_to_file(&bindings_path)
        .unwrap_or_else(|_| panic!("Failed to write {}", bindings_path.display()));
}

fn main() {
    println!("cargo:rerun-if-env-changed=CLANG_PATH");
    println!("cargo:rerun-if-env-changed=CC");

    // Get the API version and panic if more than one is declared
    #[allow(unused_mut)]
    let mut api_version: Option<u32> = None;
    version!(api_version, "fuse_31", 31);
    version!(api_version, "fuse_35", 35);
    version!(api_version, "fuse_312", 312);
    // Warn if no API version is selected
    // if api_version.is_none() {
    //     println!(
    //         "cargo:warning=No FUSE API version feature selected. Defaulting to version {}.",
    //         FUSE_DEFAULT_API_VERSION
    //     );
    // }
    // Fall back to default version
    let api_version = api_version.unwrap_or(FUSE_DEFAULT_API_VERSION);

    let mut pkgcfg = pkg_config::Config::new();
    pkgcfg.cargo_metadata(false);
    if cfg!(feature = "fuse_312") {
        pkgcfg.atleast_version("3.12.0");
    }

    // FUSE 3.1 and later use the fuse3 pkg-config module.
    let fuse_lib = pkgcfg.cargo_metadata(true).probe("fuse3").unwrap();

    // Generate highlevel bindings
    #[cfg(feature = "fuse_highlevel")]
    generate_fuse_bindings("fuse.h", api_version, &fuse_lib, fuse_binding_filter);
    // Generate lowlevel bindings
    #[cfg(feature = "fuse_lowlevel")]
    generate_fuse_bindings(
        "fuse_lowlevel.h",
        api_version,
        &fuse_lib,
        fuse_binding_filter,
    );
    // Generate lowlevel cuse bindings
    #[cfg(feature = "cuse_lowlevel")]
    generate_fuse_bindings(
        "cuse_lowlevel.h",
        api_version,
        &fuse_lib,
        cuse_binding_filter,
    );
}

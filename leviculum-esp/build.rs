use std::process::Command;

fn main() {
    emit_build_sha();
    link_bmv080_sdk();
    compile_bme690();
}

// When the `bmv080-sdk` feature is on, the firmware calls into Bosch's
// pre-compiled sensor library. The SDK is a licensed download and is not
// vendored, so the build is told where it lives rather than expecting it
// in-tree: `BMV080_SDK` points at the unpacked SDK root (the directory
// containing `api/`), falling back to the conventional docs/sensors drop.
//
// Link order is load-bearing for static archives: `bmv080` references
// `postProcessor_*`, and both reference `libm`; `libc`/`libgcc` come from
// the xtensa-esp-elf-gcc driver itself, which is already the linker.
fn link_bmv080_sdk() {
    if std::env::var_os("CARGO_FEATURE_BMV080_SDK").is_none() {
        return;
    }
    let sdk = std::env::var("BMV080_SDK").unwrap_or_else(|_| {
        format!(
            "{}/../docs/sensors/bmv080/bmv080-sdk-v11-2-0",
            std::env::var("CARGO_MANIFEST_DIR").unwrap()
        )
    });
    let lib_dir = format!("{sdk}/api/lib/xtensa_esp32s3/xtensa_esp32s3_elf_gcc/release");
    println!("cargo:rustc-link-search=native={lib_dir}");
    // The archives are named `lib_bmv080.a` / `lib_postProcessor.a`, and
    // rustc derives the filename itself (`lib<name>.a`), so the names
    // carry their leading underscores.
    println!("cargo:rustc-link-lib=static=_bmv080");
    println!("cargo:rustc-link-lib=static=_postProcessor");
    // `libm`/`libc`/`libgcc` are sysroot libraries — they live in the
    // GCC toolchain's newlib, not in rustc's own search path, so
    // `rustc-link-lib` would fail the lookup. As raw link-args the
    // xtensa-esp-elf-gcc driver (already the linker) resolves them
    // itself; they are needed explicitly because the no-std link already
    // passes `-nodefaultlibs`, and the blob calls newlib (`memcpy`,
    // `snprintf`, `pow`, …).
    println!("cargo:rustc-link-arg=-lm");
    println!("cargo:rustc-link-arg=-lc");
    println!("cargo:rustc-link-arg=-lgcc");
    println!("cargo:rerun-if-env-changed=BMV080_SDK");
}

// When the `bme690` feature is on, compile Bosch's BME690 SensorAPI
// (`bme69x.c`, BSD-3 licensed source, not a blob) plus our `csrc` glue
// into a static library. The SDK root comes from `BME690_SDK` or the
// in-tree docs/sensors drop; the compiler is xtensa-esp32s3-elf-gcc, on
// PATH whenever the esp toolchain is sourced.
fn compile_bme690() {
    if std::env::var_os("CARGO_FEATURE_BME690").is_none() {
        return;
    }
    let sdk = std::env::var("BME690_SDK").unwrap_or_else(|_| {
        format!(
            "{}/../docs/sensors/bme690/BME690_SensorAPI",
            std::env::var("CARGO_MANIFEST_DIR").unwrap()
        )
    });
    cc::Build::new()
        .compiler("xtensa-esp32s3-elf-gcc")
        .include(&sdk)
        .include("csrc")
        .file(format!("{sdk}/bme69x.c"))
        .file("csrc/bme69x_glue.c")
        // Xtensa's short `call8` can't reach across the IRAM/IROM split:
        // without -mlongcalls every call to libgcc/newlib resolves as a
        // "dangerous relocation: call target out of range" at link time.
        .flag("-mlongcalls")
        .opt_level(2)
        .compile("bme69x");
    // The non-FPU driver still does its compensation math in `double`,
    // so it wants libgcc soft-float and newlib the same way the BMV080
    // blob does — resolved by the GCC driver, not rustc's search path.
    println!("cargo:rustc-link-arg=-lm");
    println!("cargo:rustc-link-arg=-lc");
    println!("cargo:rustc-link-arg=-lgcc");
    println!("cargo:rerun-if-env-changed=BME690_SDK");
    println!("cargo:rerun-if-changed=csrc/bme69x_glue.c");
}

// Embed the short git SHA and a dirty flag so the firmware can log a
// [FW_BUILD] banner the rig reads back over the USB serial after flashing.
// This is the verification half of an auto-flash: it proves the firmware on
// the board was built from the commit under test, not a stale image.
// Best-effort: a source tree with no reachable git (release tarball) yields
// sha "unknown" and dirty=false.
//
// Byte-identical in behaviour to `leviculum-nrf/build.rs`, because the thing
// that reads the result is the same host-side tooling.
fn emit_build_sha() {
    let sha = run_git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());

    // Tracked-file modifications only. Untracked artifacts (target/, editor
    // temp files) must not flag a clean commit as dirty.
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    println!("cargo:rustc-env=LEVICULUM_GIT_SHA={sha}");
    println!("cargo:rustc-env=LEVICULUM_GIT_DIRTY={dirty}");

    // Re-run build.rs when HEAD moves so a re-flash from a new commit
    // re-embeds its SHA. Resolve the git dir explicitly to stay correct
    // under worktrees and submodules.
    if let Some(git_dir) = run_git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/index");
    }
    // Emitting ANY rerun-if-changed replaces cargo's default "rerun on any
    // package file change", so the sources have to be listed explicitly.
    // Without this an edit under `src/` left the embedded `dirty` flag stale,
    // and `[FW_BUILD]` then claimed a clean tree for an image built from
    // modified sources.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
}

fn run_git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

// SPDX-License-Identifier: LicenseRef-Brefwiz-Proprietary
//! Make jemalloc heap profiling link on static musl, without asking consumers
//! to know any of this.
//!
//! jemalloc built with `--enable-prof-libunwind` calls `unw_backtrace`. On a
//! statically linked musl target that is not a matter of adding `-lunwind`:
//!
//!   * rustc ships its own LLVM libunwind for musl in `self-contained/`, which
//!     defines the same `_Unwind_*` symbols as the system nongnu libunwind, so
//!     linking the system archive wholesale collides on every one of them.
//!   * `unw_backtrace` lives in the generic `libunwind.a`, not the
//!     arch-specific one, and is pulled in on demand — by the time the linker
//!     reaches a `-l` added at the end of the line, jemalloc's reference from
//!     an earlier rlib has already gone unsatisfied.
//!   * the system libunwind is not self-contained: it is built with
//!     minidebuginfo support and needs liblzma and libz at static link time.
//!
//! Every one of those was found by a CI gate after three releases shipped heap
//! profiling that segfaulted in production.
//!
//! Rather than push that incantation into each service's build, this extracts
//! the single archive member defining `unw_backtrace` and republishes it as a
//! small static library of our own. `rustc-link-lib` and `rustc-link-search`
//! propagate to the final binary link, so a consumer enabling
//! `profiling-memory-jemalloc` gets a working link with no build changes,
//! no flags, and no knowledge of any of the above.
//!
//! Non-musl targets are untouched: there libunwind links normally.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

/// Where a musl-built libunwind is expected to live, most specific first.
/// Alpine installs into /usr/lib; a from-source cross build for the CI image
/// conventionally lands under a per-target prefix.
fn search_dirs(target: &str) -> Vec<PathBuf> {
    let arch = target.split('-').next().unwrap_or("x86_64");
    let mut dirs = Vec::new();
    if let Ok(explicit) = env::var("OTEL_BOOTSTRAP_MUSL_LIBUNWIND_DIR") {
        dirs.push(PathBuf::from(explicit));
    }
    dirs.push(PathBuf::from(format!("/usr/local/musl/{arch}/lib")));
    dirs.push(PathBuf::from(format!("/usr/lib/{arch}-linux-musl")));
    dirs.push(PathBuf::from("/usr/lib"));
    dirs
}

/// Archive member that defines `unw_backtrace`, if any.
fn member_defining_unw_backtrace(archive: &Path) -> Option<String> {
    let out = Command::new("nm")
        .arg("--print-armap")
        .arg(archive)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // `nm --print-armap` lists "<symbol> in <member>" for the archive index.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("unw_backtrace in ")?;
            Some(rest.trim().to_owned())
        })
}

fn main() {
    println!("cargo:rerun-if-env-changed=OTEL_BOOTSTRAP_MUSL_LIBUNWIND_DIR");

    // Only relevant when heap profiling is compiled in.
    if env::var("CARGO_FEATURE_PROFILING_MEMORY_JEMALLOC").is_err() {
        return;
    }
    // ...and only on musl, where rustc's bundled unwinder conflicts. Everywhere
    // else the ordinary -lunwind that tikv-jemalloc-sys emits is sufficient.
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("musl") {
        return;
    }

    let target = env::var("TARGET").unwrap_or_default();
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    let Some((dir, archive)) = search_dirs(&target).into_iter().find_map(|d| {
        let a = d.join("libunwind.a");
        a.is_file().then(|| (d, a))
    }) else {
        // Not fatal: the link will fail with a clear undefined reference to
        // unw_backtrace, and saying so here points at the cause rather than
        // leaving the linker to.
        println!(
            "cargo:warning=otel-bootstrap: heap profiling is enabled for a musl \
             target but no musl-built libunwind.a was found. Install one, or set \
             OTEL_BOOTSTRAP_MUSL_LIBUNWIND_DIR. Without it the link fails with \
             `undefined reference to unw_backtrace`."
        );
        return;
    };

    let Some(member) = member_defining_unw_backtrace(&archive) else {
        println!(
            "cargo:warning=otel-bootstrap: {} defines no unw_backtrace; heap \
             profiling will not link for this target.",
            archive.display()
        );
        return;
    };

    // Extract just that member and repackage it. Taking the whole archive would
    // duplicate rustc's own _Unwind_* symbols; taking one member takes only what
    // jemalloc references.
    let work = out_dir.join("unwind-shim");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).expect("create shim dir");

    let extracted = Command::new("ar")
        .current_dir(&work)
        .arg("x")
        .arg(&archive)
        .arg(&member)
        .status();
    if !matches!(extracted, Ok(s) if s.success()) {
        println!(
            "cargo:warning=otel-bootstrap: could not extract {member} from {}",
            archive.display()
        );
        return;
    }

    let shim = work.join("libunwind_backtrace_shim.a");
    let packed = Command::new("ar")
        .current_dir(&work)
        .arg("rcs")
        .arg("libunwind_backtrace_shim.a")
        .arg(&member)
        .status();
    if !matches!(packed, Ok(s) if s.success()) || !shim.is_file() {
        println!("cargo:warning=otel-bootstrap: could not package the unwind shim");
        return;
    }

    // These propagate to the final binary link, which raw link-args do not —
    // that is the whole reason this is a build script rather than advice in a
    // README that every service has to follow.
    println!("cargo:rustc-link-search=native={}", work.display());
    println!("cargo:rustc-link-search=native={}", dir.display());

    // Order matters, and cargo preserves the order these are emitted in.
    //
    // The shim goes first: it is a plain object, so it links unconditionally
    // and creates a *fresh* demand for unw_backtrace's own dependencies
    // (_ULx86_64_init_local, _Ux86_64_getcontext_trace, ...) at this point in
    // the line — after jemalloc's reference, which is the demand that could
    // never be satisfied by anything appended later.
    println!("cargo:rustc-link-lib=static=unwind_backtrace_shim");

    // Then the archives, demand-loaded. This is deliberately NOT
    // --whole-archive: the members carrying _ULx86_64_* are libunwind-private
    // and pull in cleanly, while UnwindLevel1-gcc-ext.o — which would collide
    // with rustc's bundled LLVM libunwind on every _Unwind_* symbol — is never
    // demanded, because rustc's own unwinder already satisfied those earlier.
    let arch = target.split('-').next().unwrap_or("x86_64");
    println!("cargo:rustc-link-lib=static=unwind");
    println!("cargo:rustc-link-lib=static=unwind-{arch}");

    // libunwind is built with minidebuginfo, so it reads LZMA-compressed
    // .gnu_debugdata and needs these at static link time.
    println!("cargo:rustc-link-lib=static=lzma");
    println!("cargo:rustc-link-lib=static=z");
}

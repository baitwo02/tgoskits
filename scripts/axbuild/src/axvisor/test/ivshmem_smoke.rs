//! Cross-compilation of the ivshmem userspace smoke binary.
//!
//! The shared ivshmem QEMU case runs a statically linked smoke program from
//! the generated BusyBox initramfs. The program and its adapter library live
//! in `apps/linux/ivshmem/`; this module cross-compiles them with the same
//! `{arch}-linux-musl-gcc` toolchain the ArceOS C builds use and hands the
//! binary to the initramfs builder. No guest artifact is stored in git: the
//! build always runs from the current sources into the target directory.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail};

use crate::{arceos::cbuild::cc_for_arch, support::process::ProcessExt};

/// Build-group environment variable that requests the smoke binary inside
/// the generated initramfs.
pub(super) const IVSHMEM_SMOKE_ENV: &str = "AXVISOR_TEST_IVSHMEM_SMOKE";

/// Guest path of the smoke binary inside the generated initramfs.
pub(super) const SMOKE_ARCHIVE_PATH: &str = "bin/ivshmem-bar2-smoke";

const ADAPTER_SOURCES: &[&str] = &["discovery.c", "backend_polling.c", "errors.c"];
const SMOKE_SOURCE: &str = "bar2_smoke/main.c";
const SMOKE_BINARY_NAME: &str = "ivshmem-bar2-smoke";

/// Adapter compile flags: C11, size-optimized, warnings fatal. The adapter
/// must stay warning-clean so a profile regression cannot hide behind noise.
const ADAPTER_CFLAGS: &[&str] = &[
    "-std=c11",
    "-Os",
    "-g0",
    "-Wall",
    "-Wextra",
    "-Werror",
    "-ffunction-sections",
    "-fdata-sections",
];

/// Production guest binaries are statically linked: the musl toolchain
/// always ships a static libc, so the initramfs needs no loader for it.
const SMOKE_STATIC_LINK_FLAGS: &[&str] = &["-static", "-Wl,--gc-sections"];

/// Builds the statically linked smoke binary for `arch` and returns its
/// bytes, recompiling from the current sources on every call.
pub(super) fn build_smoke_binary(workspace_root: &Path, arch: &str) -> anyhow::Result<Vec<u8>> {
    build_smoke_binary_with(
        workspace_root,
        arch,
        &cc_for_arch(arch),
        SMOKE_STATIC_LINK_FLAGS,
    )
}

fn build_smoke_binary_with(
    workspace_root: &Path,
    arch: &str,
    compiler: &str,
    link_flags: &[&str],
) -> anyhow::Result<Vec<u8>> {
    if arch != "aarch64" {
        bail!("the ivshmem smoke binary is currently built for aarch64 only");
    }
    let adapter_dir = workspace_root.join("apps/linux/ivshmem");
    let lib_dir = adapter_dir.join("lib");
    // Keying the object directory by compiler keeps host-compiler build
    // tests from clobbering cross-built guest artifacts.
    let out_dir = workspace_root
        .join("target/axbuild/ivshmem-smoke")
        .join(arch)
        .join(compiler.replace('/', "_"));
    fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create {}", out_dir.display()))?;

    let mut cflags: Vec<String> = ADAPTER_CFLAGS
        .iter()
        .map(|flag| (*flag).to_string())
        .collect();
    cflags.push(format!("-I{}", lib_dir.display()));

    let mut objects = Vec::new();
    for source in ADAPTER_SOURCES {
        objects.push(compile_c_source(
            compiler,
            &cflags,
            &lib_dir.join(source),
            &out_dir,
        )?);
    }
    let smoke_object =
        compile_c_source(compiler, &cflags, &adapter_dir.join(SMOKE_SOURCE), &out_dir)?;

    let archive = out_dir.join("libivshmem.a");
    archive_objects(arch, compiler, &archive, &objects)
        .context("failed to archive the ivshmem adapter library")?;

    let binary = out_dir.join(SMOKE_BINARY_NAME);
    let mut link = Command::new(compiler);
    link.arg("-o").arg(&binary).arg(&smoke_object).arg(&archive);
    link.args(link_flags);
    link.exec()
        .with_context(|| format!("failed to link {}", binary.display()))?;

    fs::read(&binary).with_context(|| format!("failed to read {}", binary.display()))
}

fn compile_c_source(
    compiler: &str,
    cflags: &[String],
    source: &Path,
    out_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("invalid C source filename")?;
    let object = out_dir.join(format!("{stem}.o"));
    let mut command = Command::new(compiler);
    command.args(cflags);
    command.arg("-c").arg("-o").arg(&object).arg(source);
    command
        .exec()
        .with_context(|| format!("failed to compile {}", source.display()))?;
    Ok(object)
}

fn archive_objects(
    arch: &str,
    compiler: &str,
    archive: &Path,
    objects: &[PathBuf],
) -> anyhow::Result<()> {
    // The arch-specific ar comes from the same toolchain as the compiler;
    // derive its name from the compiler prefix so a custom compiler keeps a
    // matching archiver.
    let archiver = compiler
        .strip_suffix("-gcc")
        .map(|prefix| format!("{prefix}-ar"))
        .unwrap_or_else(|| format!("{arch}-linux-musl-ar"));
    let mut command = Command::new(archiver);
    command.arg("rcs").arg(archive).args(objects);
    command.exec()
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn has_host_cc() -> bool {
        Command::new("cc")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn adapter_sources_build_and_link_with_the_host_compiler() {
        if !has_host_cc() {
            panic!("a host C compiler is required for the adapter build test");
        }
        let workspace_root = crate::context::workspace_root_path().unwrap();
        // The host compiler exercises the same sources, flags, and archive
        // layout; it links dynamically because host environments do not all
        // ship a static libc. The static cross build runs in the QEMU case
        // itself.
        let binary =
            build_smoke_binary_with(&workspace_root, "aarch64", "cc", &[]).expect("smoke binary");
        assert!(binary.len() > 1024);
    }

    #[test]
    fn unsupported_architectures_are_rejected() {
        let workspace_root = crate::context::workspace_root_path().unwrap();
        let result = build_smoke_binary(&workspace_root, "riscv64");
        assert!(result.is_err());
    }
}

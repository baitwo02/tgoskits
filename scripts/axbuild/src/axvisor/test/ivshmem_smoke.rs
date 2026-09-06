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

use anyhow::{Context, bail, ensure};

use crate::{arceos::cbuild::cc_for_arch, support::process::ProcessExt};

/// Build-group environment variable that requests the smoke binary inside
/// the generated initramfs.
pub(super) const IVSHMEM_SMOKE_ENV: &str = "AXVISOR_TEST_IVSHMEM_SMOKE";

/// Build-group environment variable pointing at kernel-matched UIO modules.
pub(super) const IVSHMEM_UIO_MODULE_DIR_ENV: &str = "AXVISOR_TEST_IVSHMEM_UIO_MODULE_DIR";

/// Build-group environment variable selecting the ArceOS peer build config.
pub(super) const IVSHMEM_ARCEOS_SMOKE_ENV: &str = "AXVISOR_TEST_IVSHMEM_ARCEOS_SMOKE";

/// Guest paths of the UIO core and ivshmem PCI modules.
pub(super) const UIO_CORE_ARCHIVE_PATH: &str = "lib/modules/uio.ko";
pub(super) const UIO_IVSHMEM_ARCHIVE_PATH: &str = "lib/modules/uio_ivshmem.ko";

/// Guest paths of the legacy smoke and consolidated suite binaries.
pub(super) const SMOKE_ARCHIVE_PATH: &str = "bin/ivshmem-bar2-smoke";
pub(super) const SUITE_ARCHIVE_PATH: &str = "bin/ivshmem-pci-suite";

const ADAPTER_SOURCES: &[&str] = &["discovery.c", "backend_polling.c", "errors.c"];
const SMOKE_SOURCE: &str = "bar2_smoke/main.c";
const SMOKE_BINARY_NAME: &str = "ivshmem-bar2-smoke";
const SUITE_SOURCE: &str = "suite/main.c";
const SUITE_BINARY_NAME: &str = "ivshmem-pci-suite";

pub(super) struct SmokeBinaries {
    pub(super) legacy: Vec<u8>,
    pub(super) suite: Vec<u8>,
}

pub(super) struct UioModules {
    pub(super) core: Vec<u8>,
    pub(super) ivshmem: Vec<u8>,
}

/// Builds the ArceOS peer image from current sources before Axvisor embeds
/// VM images. Calling the current xtask executable preserves the project's
/// supported build entry and avoids a stale target artifact.
pub(super) fn build_arceos_smoke(
    workspace_root: &Path,
    arch: &str,
    configured_build: &str,
) -> anyhow::Result<()> {
    ensure!(
        arch == "aarch64",
        "the ArceOS ivshmem peer is currently built for aarch64 only"
    );
    let configured_build =
        workspace_relative_path(workspace_root, configured_build, IVSHMEM_ARCEOS_SMOKE_ENV)?;
    ensure!(
        configured_build.is_file(),
        "{} does not exist",
        configured_build.display()
    );
    let xtask = std::env::current_exe().context("failed to locate the running xtask")?;
    let mut command = Command::new(xtask);
    command
        .current_dir(workspace_root)
        .arg("arceos")
        .arg("build")
        .arg("--package")
        .arg("arceos-ivshmem-pci")
        .arg("--config")
        .arg(&configured_build);
    command
        .exec()
        .context("failed to build the ArceOS ivshmem peer")?;

    let image =
        workspace_root.join("target/aarch64-unknown-linux-musl/release/arceos-ivshmem-pci.bin");
    ensure!(
        image.is_file(),
        "ArceOS ivshmem build did not produce {}",
        image.display()
    );
    Ok(())
}

fn workspace_relative_path(
    workspace_root: &Path,
    configured: &str,
    variable: &str,
) -> anyhow::Result<PathBuf> {
    let configured = Path::new(configured);
    if configured.is_absolute()
        || !configured.components().all(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::Normal(_)
            )
        })
    {
        bail!("{variable} must be a workspace-relative path without parent traversal");
    }
    Ok(workspace_root.join(configured))
}

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

/// Reads UIO modules produced by the same build as the configured guest
/// kernel. The directory is workspace-relative so test configurations cannot
/// silently consume host-global modules with an unrelated vermagic.
pub(super) fn read_uio_modules(
    workspace_root: &Path,
    configured_dir: &str,
) -> anyhow::Result<UioModules> {
    let module_dir =
        workspace_relative_path(workspace_root, configured_dir, IVSHMEM_UIO_MODULE_DIR_ENV)?;
    let read_module = |name: &str| {
        let path = module_dir.join(name);
        fs::read(&path).with_context(|| format!("failed to read {}", path.display()))
    };
    Ok(UioModules {
        core: read_module("uio.ko")?,
        ivshmem: read_module("uio_ivshmem.ko")?,
    })
}

/// Builds both statically linked ivshmem guest programs for `arch`.
///
/// The legacy binary remains available while the consolidated case is
/// validated against the previous cases. Both programs link the same adapter
/// archive so device discovery and backend semantics cannot drift.
pub(super) fn build_smoke_binaries(
    workspace_root: &Path,
    arch: &str,
) -> anyhow::Result<SmokeBinaries> {
    build_smoke_binaries_with(
        workspace_root,
        arch,
        &cc_for_arch(arch),
        SMOKE_STATIC_LINK_FLAGS,
    )
}

fn build_smoke_binaries_with(
    workspace_root: &Path,
    arch: &str,
    compiler: &str,
    link_flags: &[&str],
) -> anyhow::Result<SmokeBinaries> {
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
    let suite_out_dir = out_dir.join("suite");
    fs::create_dir_all(&suite_out_dir)
        .with_context(|| format!("failed to create {}", suite_out_dir.display()))?;
    let suite_object = compile_c_source(
        compiler,
        &cflags,
        &adapter_dir.join(SUITE_SOURCE),
        &suite_out_dir,
    )?;

    let archive = out_dir.join("libivshmem.a");
    archive_objects(arch, compiler, &archive, &objects)
        .context("failed to archive the ivshmem adapter library")?;

    let legacy = link_smoke_binary(
        compiler,
        &out_dir.join(SMOKE_BINARY_NAME),
        &smoke_object,
        &archive,
        link_flags,
    )?;
    let suite = link_smoke_binary(
        compiler,
        &out_dir.join(SUITE_BINARY_NAME),
        &suite_object,
        &archive,
        link_flags,
    )?;
    Ok(SmokeBinaries { legacy, suite })
}

fn link_smoke_binary(
    compiler: &str,
    binary: &Path,
    object: &Path,
    archive: &Path,
    link_flags: &[&str],
) -> anyhow::Result<Vec<u8>> {
    let mut link = Command::new(compiler);
    link.arg("-o").arg(binary).arg(object).arg(archive);
    link.args(link_flags);
    link.exec()
        .with_context(|| format!("failed to link {}", binary.display()))?;
    fs::read(binary).with_context(|| format!("failed to read {}", binary.display()))
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
        let binaries = build_smoke_binaries_with(&workspace_root, "aarch64", "cc", &[])
            .expect("smoke binaries");
        assert!(binaries.legacy.len() > 1024);
        assert!(binaries.suite.len() > 1024);
    }

    #[test]
    fn adapter_behavior_tests_run_against_fixture_sysfs_and_uio() {
        if !has_host_cc() {
            panic!("a host C compiler is required for the adapter behavior test");
        }
        let workspace_root = crate::context::workspace_root_path().unwrap();
        let adapter_dir = workspace_root.join("apps/linux/ivshmem");
        let lib_dir = adapter_dir.join("lib");
        let out_dir = workspace_root.join("target/axbuild/ivshmem-smoke/host-tests");
        fs::create_dir_all(&out_dir).unwrap();
        let binary = out_dir.join("adapter-test");
        let mut command = Command::new("cc");
        command.args(ADAPTER_CFLAGS);
        command.arg(format!("-I{}", lib_dir.display()));
        for source in ADAPTER_SOURCES {
            command.arg(lib_dir.join(source));
        }
        command.arg(lib_dir.join("tests/adapter_test.c"));
        command.arg("-o").arg(&binary);
        command.exec().expect("compile adapter behavior test");
        Command::new(&binary)
            .exec()
            .expect("run adapter behavior test");
    }

    #[test]
    fn uio_modules_must_come_from_a_workspace_relative_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let module_dir = workspace.path().join("kernel-modules");
        fs::create_dir(&module_dir).unwrap();
        fs::write(module_dir.join("uio.ko"), b"uio").unwrap();
        fs::write(module_dir.join("uio_ivshmem.ko"), b"ivshmem").unwrap();

        let modules = read_uio_modules(workspace.path(), "kernel-modules").unwrap();
        assert_eq!(modules.core, b"uio");
        assert_eq!(modules.ivshmem, b"ivshmem");
        assert!(read_uio_modules(workspace.path(), "../outside").is_err());
        assert!(read_uio_modules(workspace.path(), "/outside").is_err());
    }

    #[test]
    fn arceos_smoke_build_config_must_stay_inside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        assert_eq!(
            workspace_relative_path(
                workspace.path(),
                "apps/arceos/build.toml",
                IVSHMEM_ARCEOS_SMOKE_ENV,
            )
            .unwrap(),
            workspace.path().join("apps/arceos/build.toml")
        );
        assert!(
            workspace_relative_path(workspace.path(), "../outside", IVSHMEM_ARCEOS_SMOKE_ENV)
                .is_err()
        );
        assert!(
            workspace_relative_path(workspace.path(), "/outside", IVSHMEM_ARCEOS_SMOKE_ENV)
                .is_err()
        );
    }

    #[test]
    fn unsupported_architectures_are_rejected() {
        let workspace_root = crate::context::workspace_root_path().unwrap();
        assert!(build_smoke_binaries(&workspace_root, "riscv64").is_err());
        assert!(build_arceos_smoke(&workspace_root, "riscv64", "unused.toml").is_err());
    }
}

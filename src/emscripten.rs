//! Locating or installing the Emscripten toolchain (`emcc`).
//!
//! The emscripten build needs `emcc` resolvable by rustc (which drives it as
//! the linker). Resolution order:
//!
//!   1. `emcc` already on `PATH` — the user's toolchain wins, untouched.
//!   2. `$EMSDK` pointing at an installed + activated emsdk.
//!   3. `~/emsdk` (the conventional install location).
//!   4. A wasm-pack-managed install in the wasm-pack cache.
//!   5. With installation permitted (and confirmed on a tty), install into
//!      the wasm-pack cache: the pinned emsdk release for the
//!      LLVM/binaryen/node toolchain, plus emscripten `main` (carrying the
//!      post-link `-sWASM_BINDGEN` support) overlaid on top.
//!
//! All environment adjustments are process-scoped: they apply only to the
//! `cargo` child wasm-pack spawns, so no shell activation (`emsdk_env.sh`)
//! is ever required.

use crate::PBAR;
use anyhow::{anyhow, bail, Context, Result};
use binary_install::Cache;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The pinned emsdk release installed into the wasm-pack cache. Supplies
/// LLVM, binaryen, node, and (on Windows) python.
const EMSDK_VERSION: &str = "6.0.9";

/// The emscripten branch carrying post-link `-sWASM_BINDGEN` support
/// (emscripten-core/emscripten#27208), overlaid on the emsdk toolchain.
// TODO: drop the overlay once emscripten 6.0.10 is tagged and available via
// emsdk; the emsdk toolchain then serves emcc directly.
const EMSCRIPTEN_OVERLAY_REPO: &str = "https://github.com/emscripten-core/emscripten";
const EMSCRIPTEN_OVERLAY_BRANCH: &str = "main";

/// Stamp file marking a completed install phase.
const READY_STAMP: &str = ".wasm-pack-ready";

/// Environment adjustments for spawning `cargo` such that rustc can drive
/// `emcc` as the linker.
#[derive(Default)]
pub struct EmccEnv {
    /// Directories to prepend to `PATH`.
    pub path_prepends: Vec<PathBuf>,
    /// Extra environment variables (`EM_CONFIG`, `EMSDK`).
    pub vars: Vec<(&'static str, PathBuf)>,
}

/// Locate `emcc`, installing the toolchain into the wasm-pack cache if
/// permitted and confirmed.
pub fn ensure_emcc(cache: &Cache, install_permitted: bool) -> Result<EmccEnv> {
    // 1. The user's own emcc.
    if which::which("emcc").is_ok() {
        return Ok(EmccEnv::default());
    }

    // 2. / 3. An existing emsdk install (activated), via $EMSDK or ~/emsdk.
    let candidates = std::env::var_os("EMSDK")
        .map(PathBuf::from)
        .into_iter()
        .chain(dirs::home_dir().map(|home| home.join("emsdk")));
    for dir in candidates {
        if let Some(env) = emsdk_env(&dir, None) {
            PBAR.info(&format!("Using the Emscripten SDK at {}", dir.display()));
            return Ok(env);
        }
    }

    // 4. / 5. The wasm-pack-managed install.
    let emsdk_dir = cache.join(Path::new(&format!("emsdk-{}", EMSDK_VERSION)));
    let overlay_dir = cache.join(Path::new("emscripten-wasm-bindgen"));
    if !is_ready(&emsdk_dir, &overlay_dir) {
        if !install_permitted {
            bail!(
                "Targeting wasm32-unknown-emscripten requires `emcc` (the Emscripten \
                 compiler driver), which was not found on PATH, and installation is \
                 disabled (--mode no-install).\n{}",
                manual_install_instructions(),
            );
        }
        confirm_install()?;
        install(&emsdk_dir, &overlay_dir)?;
    }
    emsdk_env(&emsdk_dir, Some(&overlay_dir)).ok_or_else(|| {
        anyhow!(
            "the emsdk install at {} is not usable; delete it to reinstall",
            emsdk_dir.display()
        )
    })
}

/// Whether the wasm-pack-managed install previously completed. Checked
/// per phase (stamp plus key artifact) so a broken phase re-installs.
fn is_ready(emsdk_dir: &Path, overlay_dir: &Path) -> bool {
    emsdk_ready(emsdk_dir) && overlay_ready(overlay_dir)
}

fn emsdk_ready(emsdk_dir: &Path) -> bool {
    emsdk_dir.join(READY_STAMP).exists() && emsdk_dir.join(".emscripten").exists()
}

fn overlay_ready(overlay_dir: &Path) -> bool {
    overlay_dir.join(READY_STAMP).exists() && has_emcc(overlay_dir)
}

/// Whether `dir` contains the emcc entry point rustc will invoke as the
/// linker (`emcc.bat` on Windows — rustc hardcodes the name — `emcc`
/// elsewhere).
fn has_emcc(dir: &Path) -> bool {
    if cfg!(windows) {
        dir.join("emcc.bat").exists()
    } else {
        dir.join("emcc").exists()
    }
}

/// Build the child-process environment for an emsdk directory, with the
/// emcc directory being either the overlay checkout (wasm-pack-managed
/// installs) or emsdk's own bundled emscripten (user installs).
fn emsdk_env(emsdk_dir: &Path, overlay_dir: Option<&Path>) -> Option<EmccEnv> {
    let config = emsdk_dir.join(".emscripten");
    if !config.exists() {
        return None;
    }
    let emcc_dir = match overlay_dir {
        Some(dir) => dir.to_path_buf(),
        None => emsdk_dir.join("upstream").join("emscripten"),
    };
    if !has_emcc(&emcc_dir) {
        return None;
    }
    let mut path_prepends = vec![emcc_dir];
    let mut vars = vec![
        ("EM_CONFIG", config.clone()),
        ("EMSDK", emsdk_dir.to_path_buf()),
    ];
    // node (and on Windows, python) come from the emsdk; emcc resolves them
    // via the config, but the JS tooling it shells out to needs them on PATH.
    for key in ["NODE_JS", "PYTHON"] {
        if let Some(tool) = config_tool_path(emsdk_dir, &config, key) {
            if let Some(bin_dir) = tool.parent() {
                path_prepends.push(bin_dir.to_path_buf());
            }
            // The emcc.bat/run_python launchers resolve python via
            // EMSDK_PYTHON before falling back to PATH.
            if key == "PYTHON" {
                vars.push(("EMSDK_PYTHON", tool));
            }
        }
    }
    Some(EmccEnv {
        path_prepends,
        vars,
    })
}

/// Parse a tool path (e.g. `NODE_JS = '$CFGDIR/node/…/bin/node'`) out of an
/// emsdk-generated `.emscripten` config.
fn config_tool_path(emsdk_dir: &Path, config: &Path, key: &str) -> Option<PathBuf> {
    let body = std::fs::read_to_string(config).ok()?;
    let line = body.lines().find(|l| {
        l.trim_start()
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })?;
    let value = line.split('\'').nth(1)?;
    let path = match value
        .strip_prefix("$CFGDIR/")
        .or_else(|| value.strip_prefix("$CFGDIR\\"))
    {
        Some(rel) => emsdk_dir.join(rel),
        None => PathBuf::from(value),
    };
    Some(path)
}

/// One-time confirmation before the large toolchain download. Skipped (with
/// a notice) when unattended, so CI proceeds unprompted.
fn confirm_install() -> Result<()> {
    let msg = format!(
        "wasm-pack can install the Emscripten SDK {} into its cache (a one-time ~1.3 GB download)",
        EMSDK_VERSION
    );
    if !console::user_attended() {
        PBAR.info(&format!("{}; installing...", msg));
        return Ok(());
    }
    let confirmed = dialoguer::Confirm::new()
        .with_prompt(format!("{}. Proceed?", msg))
        .default(true)
        .interact()?;
    if !confirmed {
        bail!(
            "Emscripten SDK installation declined.\n{}",
            manual_install_instructions()
        );
    }
    Ok(())
}

fn manual_install_instructions() -> String {
    format!(
        "To install manually:\n\n\
         \tgit clone https://github.com/emscripten-core/emsdk\n\
         \tcd emsdk\n\
         \t./emsdk install {version}\n\
         \t./emsdk activate {version}\n\
         \tsource ./emsdk_env.sh\n\n\
         plus, until emscripten 6.0.10 ships, the emscripten branch carrying \
         post-link -sWASM_BINDGEN support (see the wasm-pack emscripten docs):\n\n\
         \tgit clone -b {branch} {repo}\n\
         \tcd emscripten && ./bootstrap\n",
        version = EMSDK_VERSION,
        branch = EMSCRIPTEN_OVERLAY_BRANCH,
        repo = EMSCRIPTEN_OVERLAY_REPO,
    )
}

/// Install the pinned emsdk plus the emscripten overlay into the cache.
/// Idempotent: each phase is stamped and re-run from scratch if incomplete.
fn install(emsdk_dir: &Path, overlay_dir: &Path) -> Result<()> {
    let python = python_bin()?;

    if !emsdk_ready(emsdk_dir) {
        clone_fresh(
            "https://github.com/emscripten-core/emsdk",
            EMSDK_VERSION,
            emsdk_dir,
        )?;
        PBAR.info("Installing the Emscripten toolchain (this downloads ~1.3 GB)...");
        for step in ["install", "activate"] {
            let mut cmd = Command::new(&python);
            cmd.arg(emsdk_dir.join("emsdk.py"))
                .arg(step)
                .arg(EMSDK_VERSION)
                .current_dir(emsdk_dir);
            crate::child::run(cmd, "emsdk")
                .with_context(|| format!("running `emsdk {} {}`", step, EMSDK_VERSION))?;
        }
        std::fs::write(emsdk_dir.join(READY_STAMP), "")?;
    }

    if !overlay_ready(overlay_dir) {
        clone_fresh(
            EMSCRIPTEN_OVERLAY_REPO,
            EMSCRIPTEN_OVERLAY_BRANCH,
            overlay_dir,
        )?;
        PBAR.info("Bootstrapping emscripten...");
        // npm (from the emsdk's node) must be resolvable for bootstrap.
        let config = emsdk_dir.join(".emscripten");
        let node_path_env = config_tool_path(emsdk_dir, &config, "NODE_JS")
            .and_then(|node| node.parent().map(Path::to_path_buf))
            .map(|node_bin| -> Result<_> {
                let path_var = std::env::var_os("PATH").unwrap_or_default();
                let paths = std::iter::once(node_bin)
                    .chain(std::env::split_paths(&path_var))
                    .collect::<Vec<_>>();
                Ok(std::env::join_paths(paths)?)
            })
            .transpose()?;
        let mut cmd = Command::new(&python);
        cmd.arg(overlay_dir.join("bootstrap.py"))
            .current_dir(overlay_dir);
        // rustc invokes the linker as `emcc.bat` on Windows; without this,
        // bootstrap generates pylauncher `.exe` entry points instead.
        cmd.env("EM_USE_BAT_FILES", "1");
        if let Some(path_env) = &node_path_env {
            cmd.env("PATH", path_env);
        }
        crate::child::run(cmd, "bootstrap").context("bootstrapping the emscripten checkout")?;
        std::fs::write(overlay_dir.join(READY_STAMP), "")?;
    }

    PBAR.info("Emscripten toolchain installed.");
    Ok(())
}

/// Shallow-clone `git_ref` of `repo` into `dir`, clearing any partial
/// previous attempt.
fn clone_fresh(repo: &str, git_ref: &str, dir: &Path) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    let mut cmd = Command::new("git");
    cmd.arg("clone")
        .arg("--depth")
        .arg("1")
        .arg("-b")
        .arg(git_ref)
        .arg(repo)
        .arg(dir);
    crate::child::run(cmd, "git")
        .with_context(|| format!("cloning {} (is `git` installed and on PATH?)", repo))
}

/// A python interpreter for driving emsdk.py / bootstrap.py.
fn python_bin() -> Result<PathBuf> {
    which::which("python3")
        .or_else(|_| which::which("python"))
        .map_err(|_| {
            anyhow!(
                "Installing the Emscripten SDK requires `python3` on PATH.\n{}",
                manual_install_instructions()
            )
        })
}

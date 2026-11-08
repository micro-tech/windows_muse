//! Muse for Windows — installer.
//!
//! Standalone Rust installer for the windows_muse Tauri app. It copies the
//! built `muse-windows-app.exe` into `%LOCALAPPDATA%\muse-windows-app` (no
//! admin), creates Desktop + Start Menu shortcuts, writes a fresh
//! `%APPDATA%\muse-windows-app\config.toml` from `config.example.toml` only
//! if one does not already exist, and registers an uninstaller under
//! HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall.
//!
//! It does NOT build the app from source — build it first with
//! `cargo tauri build`, then run this installer next to the built exe
//! (or pass `--from <path>`).
//!
//! Windows-only at runtime: it compiles on Linux/macOS so the repo can be
//! checked cross-platform, but `require_windows()` refuses to run there.
//! Windows-specific code (registry) is behind `cfg(windows)`; the shortcut
//! and self-delete steps shell out to PowerShell/cmd, which only exist on
//! Windows.

use anyhow::{Context, Result};
use clap::{Arg, Command};
use console::{style, Emoji};
use dialoguer::{theme::ColorfulTheme, Confirm, Input};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

#[cfg(windows)]
use winreg::enums::*;
#[cfg(windows)]
use winreg::RegKey;

static SPARKLE: Emoji<'_, '_> = Emoji("✨ ", "");
static TRUCK: Emoji<'_, '_> = Emoji("🚚 ", "");
static CLIP: Emoji<'_, '_> = Emoji("📎 ", "");
static PAPER: Emoji<'_, '_> = Emoji("📃 ", "");

const APP_NAME: &str = "Muse for Windows";
const APP_EXE: &str = "muse-windows-app.exe";
const INSTALLER_EXE: &str = "muse-installer.exe";
const INSTALL_DIR_NAME: &str = "muse-windows-app";
const CONFIG_FILE: &str = "config.toml";
const CONFIG_TEMPLATE_FILE: &str = "config.example.toml";
const SHORTCUT_NAME: &str = "Muse for Windows.lnk";
// Only *called* from cfg(windows) registry code, so a Linux build warns
// about dead code — but the tests below verify this string cross-platform,
// so it stays ungated.
#[allow(dead_code)]
const UNINSTALL_SUBKEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Muse for Windows";
const HUB_TOKEN_PLACEHOLDER: &str = "PASTE_YOUR_HUB_TOKEN_HERE";

/// The repo's `config.example.toml`, baked into the installer at build time.
/// A `config.example.toml` sitting next to the installer binary at runtime
/// wins over this embedded copy (see `load_config_template`).
const EMBEDDED_CONFIG_TEMPLATE: &str = include_str!("../../config.example.toml");

/// Refuse to run on non-Windows: every install step is Windows-only
/// (registry, PowerShell shortcuts, %LOCALAPPDATA% layout).
#[cfg(not(windows))]
fn require_windows() -> Result<()> {
    Err(anyhow::anyhow!(
        "This installer targets Windows only. It installs the Muse for Windows desktop app into %LOCALAPPDATA% and registers shortcuts + an uninstaller, which are all Windows-only."
    ))
}

#[cfg(windows)]
fn require_windows() -> Result<()> {
    Ok(())
}

/// Default install location: %LOCALAPPDATA%\muse-windows-app (no admin).
fn default_install_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\Default\AppData\Local"))
        .join(INSTALL_DIR_NAME)
}

/// Config directory: %APPDATA%\muse-windows-app.
fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\Default\AppData\Roaming"))
        .join(INSTALL_DIR_NAME)
}

/// Ordered places to look for the app exe. `--from` wins; then the exe next
/// to this installer; then the repo-layout release path relative to the
/// installer's own build dir (`installer/target/release` ->
/// `src-tauri/target/release`); then the current directory (and its
/// `src-tauri/target/release`, for a repo-root run).
fn candidate_app_exe_paths(from: Option<&Path>, installer_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = from {
        candidates.push(path.to_path_buf());
    }
    candidates.push(installer_dir.join(APP_EXE));
    candidates.push(
        installer_dir
            .join("../../../src-tauri/target/release")
            .join(APP_EXE),
    );
    candidates.push(cwd.join(APP_EXE));
    candidates.push(cwd.join("src-tauri/target/release").join(APP_EXE));
    candidates
}

/// First candidate that exists (canonicalized), or an error listing every
/// place that was checked.
fn resolve_app_exe(from: Option<&Path>, installer_dir: &Path, cwd: &Path) -> Result<PathBuf> {
    let candidates = candidate_app_exe_paths(from, installer_dir, cwd);
    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(fs::canonicalize(candidate).unwrap_or_else(|_| candidate.clone()));
        }
    }
    let checked = candidates
        .iter()
        .map(|p| format!("  - {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    Err(anyhow::anyhow!(
        "Could not find {APP_EXE} to install.\nChecked:\n{checked}\n\nBuild the app first (`cargo tauri build`), then run this installer next to the built exe, or pass --from <path>."
    ))
}

/// Load the config template: a `config.example.toml` sitting next to the
/// installer binary wins (so a fresh template can be shipped without
/// rebuilding the installer); otherwise the copy baked in at build time.
fn load_config_template(installer_dir: &Path) -> Result<String> {
    let next_to_installer = installer_dir.join(CONFIG_TEMPLATE_FILE);
    if next_to_installer.is_file() {
        return fs::read_to_string(&next_to_installer)
            .with_context(|| format!("Failed to read {}", next_to_installer.display()));
    }
    Ok(EMBEDDED_CONFIG_TEMPLATE.to_string())
}

enum ConfigOutcome {
    Created(PathBuf),
    KeptExisting(PathBuf),
}

/// Write `config.toml` into `dir` from the template — but ONLY if it does
/// not already exist. An existing config is never overwritten: it holds the
/// user's real hub token.
fn ensure_config(dir: &Path, template: &str) -> Result<ConfigOutcome> {
    let config_path = dir.join(CONFIG_FILE);
    if config_path.exists() {
        return Ok(ConfigOutcome::KeptExisting(config_path));
    }
    fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    fs::write(&config_path, template)
        .with_context(|| format!("Failed to write {}", config_path.display()))?;
    Ok(ConfigOutcome::Created(config_path))
}

/// PowerShell that creates one .lnk via WScript.Shell. Pure string building
/// so it can be unit-tested; the actual `powershell` invocation is in
/// `create_shortcut`. (Paths are single-quoted like the RustRecon installer;
/// a path containing a `'` would break the script — rare on Windows.)
fn shortcut_ps_script(
    shortcut_path: &Path,
    target: &Path,
    working_dir: &Path,
    description: &str,
) -> String {
    format!(
        r#"$WshShell = New-Object -comObject WScript.Shell
$Shortcut = $WshShell.CreateShortcut('{shortcut}')
$Shortcut.TargetPath = '{target}'
$Shortcut.WorkingDirectory = '{working}'
$Shortcut.Description = '{description}'
$Shortcut.Save()
"#,
        shortcut = shortcut_path.display(),
        target = target.display(),
        working = working_dir.display(),
        description = description,
    )
}

fn create_shortcut(shortcut_path: &Path, target: &Path, working_dir: &Path) -> Result<()> {
    let script = shortcut_ps_script(shortcut_path, target, working_dir, APP_NAME);
    let output = StdCommand::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .context("Failed to run powershell to create a shortcut")?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Failed to create shortcut {}: {}",
            shortcut_path.display(),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn desktop_shortcut_path() -> Result<PathBuf> {
    Ok(dirs::desktop_dir()
        .context("Failed to find the Desktop directory")?
        .join(SHORTCUT_NAME))
}

fn start_menu_shortcut_path() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("Failed to find the AppData directory")?
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join(SHORTCUT_NAME))
}

/// UninstallString: "<installed installer exe>" --uninstall (quoted — the
/// path under %LOCALAPPDATA% can contain spaces). Only *called* from
/// cfg(windows) registry code; the test below verifies it cross-platform.
#[allow(dead_code)]
fn uninstall_string(installer_exe: &Path) -> String {
    format!("\"{}\" --uninstall", installer_exe.display())
}

#[cfg(windows)]
fn register_uninstaller(install_dir: &Path, installer_exe: &Path, app_exe: &Path) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(UNINSTALL_SUBKEY)?;
    let install_location = install_dir.to_string_lossy();
    let display_icon = app_exe.to_string_lossy();
    key.set_value("DisplayName", &APP_NAME)?;
    key.set_value("DisplayVersion", &env!("CARGO_PKG_VERSION"))?;
    key.set_value("Publisher", &"John McConnell")?;
    key.set_value("InstallLocation", &*install_location)?;
    key.set_value("UninstallString", &uninstall_string(installer_exe))?;
    key.set_value("DisplayIcon", &*display_icon)?;
    key.set_value("NoModify", &1u32)?;
    key.set_value("NoRepair", &1u32)?;
    Ok(())
}

/// Windows-only: the uninstaller entry lives in the registry
/// (HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall).
#[cfg(not(windows))]
fn register_uninstaller(_install_dir: &Path, _installer_exe: &Path, _app_exe: &Path) -> Result<()> {
    Err(anyhow::anyhow!(
        "Registering the uninstaller requires the Windows registry — this installer targets Windows only"
    ))
}

#[cfg(windows)]
fn read_install_location() -> Result<PathBuf> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey(UNINSTALL_SUBKEY)?;
    let location: String = key.get_value("InstallLocation")?;
    Ok(PathBuf::from(location))
}

/// Windows-only: the install location is read back from the registry.
#[cfg(not(windows))]
fn read_install_location() -> Result<PathBuf> {
    Err(anyhow::anyhow!(
        "Reading the install location requires the Windows registry — this installer targets Windows only"
    ))
}

#[cfg(windows)]
fn remove_uninstaller_entry() -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.delete_subkey_all(UNINSTALL_SUBKEY)?;
    Ok(())
}

/// Windows-only: the uninstaller entry lives in the registry.
#[cfg(not(windows))]
fn remove_uninstaller_entry() -> Result<()> {
    Err(anyhow::anyhow!(
        "Removing the uninstaller entry requires the Windows registry — this installer targets Windows only"
    ))
}

/// True when `path` and `dir` are the same file/dir, by raw or canonicalized
/// path. Used to tell whether the running installer is its own installed
/// copy (so we don't try to copy it over itself).
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// True when `path` lives under `dir` (component-wise, so a sibling like
/// `muse-windows-app-2` does not match `muse-windows-app`). Used by
/// uninstall to tell whether the running installer exe is inside the install
/// dir it is about to delete.
fn exe_inside_install_dir(exe: &Path, install_dir: &Path) -> bool {
    let exe_c = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let dir_c = fs::canonicalize(install_dir).unwrap_or_else(|_| install_dir.to_path_buf());
    exe_c.starts_with(&dir_c)
}

#[derive(Debug, Clone)]
struct InstallOptions {
    install_dir: PathBuf,
    desktop_shortcut: bool,
    start_menu: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            install_dir: default_install_dir(),
            desktop_shortcut: true,
            start_menu: true,
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "Yes"
    } else {
        "No"
    }
}

fn show_welcome() {
    println!();
    println!(
        "{}",
        style("═══════════════════════════════════════").cyan()
    );
    println!(
        "{}",
        style("    Muse for Windows — Installer").cyan().bold()
    );
    println!(
        "{}",
        style("═══════════════════════════════════════").cyan()
    );
    println!();
    println!("Installs the Muse for Windows desktop app (Tauri) into");
    println!("%LOCALAPPDATA%\\muse-windows-app — no admin required.");
    println!();
}

fn gather_options() -> Result<InstallOptions> {
    let mut options = InstallOptions::default();
    println!("{}Installation options:", CLIP);

    let default_dir = options.install_dir.to_string_lossy().to_string();
    let dir: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Install to")
        .default(default_dir)
        .interact_text()?;
    options.install_dir = PathBuf::from(dir);

    options.desktop_shortcut = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Create a Desktop shortcut?")
        .default(true)
        .interact()?;

    options.start_menu = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Create a Start Menu entry?")
        .default(true)
        .interact()?;

    println!();
    Ok(options)
}

fn confirm_install(app_exe_src: &Path, options: &InstallOptions) -> Result<bool> {
    println!("{}Installation summary:", PAPER);
    println!("  📦 App exe: {}", app_exe_src.display());
    println!("  📁 Install to: {}", options.install_dir.display());
    println!(
        "  🖥️  Desktop shortcut: {}",
        yes_no(options.desktop_shortcut)
    );
    println!("  📋 Start Menu entry: {}", yes_no(options.start_menu));
    println!(
        "  🔑 Config: {} (created only if missing — never overwritten)",
        config_dir().join(CONFIG_FILE).display()
    );
    println!();

    Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Proceed with installation?")
        .default(true)
        .interact()
        .context("Failed to read confirmation")
}

fn install(app_exe_src: &Path, options: &InstallOptions, silent: bool, force: bool) -> Result<()> {
    // Already installed? Interactive mode asks before overwriting; silent
    // and --force reinstall over the top.
    let existing = options.install_dir.join(APP_EXE);
    if existing.exists() && !silent && !force {
        if !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "{APP_NAME} is already installed at {}. Reinstall (overwrite)?",
                options.install_dir.display()
            ))
            .default(true)
            .interact()?
        {
            println!("Installation cancelled.");
            return Ok(());
        }
    }

    println!("{}Installing {}...", TRUCK, APP_NAME);
    fs::create_dir_all(&options.install_dir)
        .with_context(|| format!("Failed to create {}", options.install_dir.display()))?;

    // App exe.
    let app_dst = options.install_dir.join(APP_EXE);
    fs::copy(app_exe_src, &app_dst).context("Failed to copy the app exe")?;
    println!("  ✅ Installed {}", app_dst.display());

    // The installer exe itself, so the registered UninstallString has a
    // target that still exists after the download folder is cleaned up.
    // (Skipped when this run IS the installed copy.)
    let current_exe = env::current_exe().context("Failed to locate this installer exe")?;
    let installer_dst = options.install_dir.join(INSTALLER_EXE);
    if !same_file(&current_exe, &installer_dst) {
        fs::copy(&current_exe, &installer_dst).context("Failed to copy the installer exe")?;
        println!("  ✅ Installed {}", installer_dst.display());
    }

    // Config — only if there isn't one already. The template keeps the
    // placeholder token; a real token is the user's to fill in, never
    // written by this installer.
    let installer_dir = current_exe
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let template = load_config_template(&installer_dir)?;
    match ensure_config(&config_dir(), &template)? {
        ConfigOutcome::Created(path) => {
            println!("  ✅ Created config template at {}", path.display());
            println!("     Fill in your hub token there (replace {HUB_TOKEN_PLACEHOLDER}).");
        }
        ConfigOutcome::KeptExisting(path) => {
            println!("  ✅ Kept your existing config at {}", path.display());
        }
    }

    // Shortcuts (best effort — a failed shortcut must not fail the install).
    if options.desktop_shortcut {
        match desktop_shortcut_path()
            .and_then(|p| create_shortcut(&p, &app_dst, &options.install_dir))
        {
            Ok(()) => println!("  ✅ Created Desktop shortcut"),
            Err(e) => println!("  ⚠️  Could not create Desktop shortcut: {e}"),
        }
    }
    if options.start_menu {
        match start_menu_shortcut_path()
            .and_then(|p| create_shortcut(&p, &app_dst, &options.install_dir))
        {
            Ok(()) => println!("  ✅ Created Start Menu entry"),
            Err(e) => println!("  ⚠️  Could not create Start Menu entry: {e}"),
        }
    }

    // Uninstaller registration, HKCU (no admin).
    match register_uninstaller(&options.install_dir, &installer_dst, &app_dst) {
        Ok(()) => println!("  ✅ Registered uninstaller (Settings → Apps)"),
        Err(e) => println!("  ⚠️  Could not register uninstaller: {e}"),
    }

    println!();
    println!(
        "{}",
        style("═══════════════════════════════════════").green()
    );
    println!(
        "{}",
        style(format!("    {APP_NAME} installed!")).green().bold()
    );
    println!(
        "{}",
        style("═══════════════════════════════════════").green()
    );
    println!();
    println!("{}Installed to: {}", SPARKLE, options.install_dir.display());
    println!("🚀 Launch it from the Desktop or Start Menu shortcut.");
    println!();
    println!(
        "🔑 Hub token: edit {} and replace {HUB_TOKEN_PLACEHOLDER} with your own hub token, then restart the app.",
        config_dir().join(CONFIG_FILE).display()
    );
    println!(
        "🗑️  To uninstall: run `{} --uninstall`, or use Settings → Apps → {APP_NAME} → Uninstall.",
        installer_dst.display()
    );
    println!();
    Ok(())
}

fn uninstall() -> Result<()> {
    println!("{}Uninstalling {}...", TRUCK, APP_NAME);

    // Install location from the registry; fall back to the default dir if
    // the registration is already gone.
    let install_dir = read_install_location().unwrap_or_else(|_| default_install_dir());
    println!("  📁 Install dir: {}", install_dir.display());

    // Shortcuts (best effort).
    if let Ok(path) = desktop_shortcut_path() {
        let _ = fs::remove_file(&path);
    }
    if let Ok(path) = start_menu_shortcut_path() {
        let _ = fs::remove_file(&path);
    }
    println!("  ✅ Removed Desktop + Start Menu shortcuts");

    // Registry entry (best effort).
    match remove_uninstaller_entry() {
        Ok(()) => println!("  ✅ Removed uninstaller registration"),
        Err(e) => println!("  ⚠️  Could not remove uninstaller registration: {e}"),
    }

    // Install dir. If this installer is running from inside it, Windows will
    // not let it delete its own exe — schedule a delayed rmdir via cmd and
    // exit; the dir (and this exe) disappear a couple of seconds later.
    let current_exe = env::current_exe().unwrap_or_else(|_| PathBuf::from(""));
    if !current_exe.as_os_str().is_empty() && exe_inside_install_dir(&current_exe, &install_dir) {
        let dir_arg = install_dir.to_string_lossy();
        let cleanup_cmd = format!("timeout /t 2 /nobreak >nul & rmdir /s /q \"{dir_arg}\"");
        let _ = StdCommand::new("cmd").args(["/C", &cleanup_cmd]).spawn();
        println!(
            "  ✅ Install folder will be removed in a moment: {}",
            install_dir.display()
        );
    } else if install_dir.exists() {
        fs::remove_dir_all(&install_dir)
            .with_context(|| format!("Failed to remove {}", install_dir.display()))?;
        println!("  ✅ Removed {}", install_dir.display());
    }

    println!();
    println!("✅ {APP_NAME} has been uninstalled.");
    println!(
        "   (Your hub config at {} was left alone — delete it by hand if you want it gone.)",
        config_dir().join(CONFIG_FILE).display()
    );
    Ok(())
}

fn main() -> Result<()> {
    let matches = Command::new("muse-installer")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Installs Muse for Windows (Tauri desktop app) — no admin required")
        .arg(
            Arg::new("silent")
                .long("silent")
                .short('s')
                .help("Run in silent mode with default options (no prompts)")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("uninstall")
                .long("uninstall")
                .help("Uninstall Muse for Windows")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("force")
                .long("force")
                .short('f')
                .help("Reinstall over an existing install without asking")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("from")
                .long("from")
                .value_name("PATH")
                .help("Path to the muse-windows-app.exe to install (default: look next to this installer, then the repo's src-tauri/target/release)")
                .action(clap::ArgAction::Set),
        )
        .get_matches();

    // Fail fast on non-Windows: every step below is Windows-only.
    require_windows()?;

    if matches.get_flag("uninstall") {
        return uninstall();
    }

    let silent = matches.get_flag("silent");
    let force = matches.get_flag("force");
    let from = matches.get_one::<String>("from").map(PathBuf::from);

    // Locate the app exe before prompting, so a missing exe fails fast with
    // a clear error instead of after the user answered prompts.
    let current_exe = env::current_exe().context("Failed to locate this installer exe")?;
    let installer_dir = current_exe
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let app_exe_src = resolve_app_exe(from.as_deref(), &installer_dir, &cwd)?;

    show_welcome();

    let options = if silent {
        println!("Running silent installation with default options...");
        InstallOptions::default()
    } else {
        let options = gather_options()?;
        if !confirm_install(&app_exe_src, &options)? {
            println!("Installation cancelled.");
            return Ok(());
        }
        options
    };

    install(&app_exe_src, &options, silent, force)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A fresh scratch dir under the system temp dir (removed by the test).
    fn scratch_dir(tag: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = env::temp_dir().join(format!(
            "muse-installer-test-{}-{}-{}",
            std::process::id(),
            n,
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn write_fake_exe(dir: &Path) -> PathBuf {
        let exe = dir.join(APP_EXE);
        fs::write(&exe, b"MZ fake exe").expect("write fake exe");
        exe
    }

    #[test]
    fn default_install_dir_ends_with_app_folder() {
        assert_eq!(default_install_dir().file_name().unwrap(), INSTALL_DIR_NAME);
    }

    #[test]
    fn config_dir_ends_with_app_folder() {
        assert_eq!(config_dir().file_name().unwrap(), INSTALL_DIR_NAME);
    }

    #[test]
    fn resolve_app_exe_prefers_from() {
        let root = scratch_dir("from-wins");
        let from_dir = root.join("from");
        let installer_dir = root.join("installer");
        let cwd = root.join("cwd");
        for d in [&from_dir, &installer_dir, &cwd] {
            fs::create_dir_all(d).unwrap();
        }
        let from_exe = write_fake_exe(&from_dir);
        write_fake_exe(&installer_dir); // a competing exe next to the installer
        let found = resolve_app_exe(Some(&from_exe), &installer_dir, &cwd).unwrap();
        assert_eq!(found, fs::canonicalize(&from_exe).unwrap());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resolve_app_exe_finds_exe_next_to_installer() {
        let root = scratch_dir("next-to-installer");
        let installer_dir = root.join("installer");
        let cwd = root.join("cwd");
        fs::create_dir_all(&installer_dir).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let exe = write_fake_exe(&installer_dir);
        let found = resolve_app_exe(None, &installer_dir, &cwd).unwrap();
        assert_eq!(found, fs::canonicalize(&exe).unwrap());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resolve_app_exe_finds_repo_layout_path() {
        // installer at <root>/installer/target/release, app exe at
        // <root>/src-tauri/target/release — the ../../../ candidate.
        let root = scratch_dir("repo-layout");
        let installer_dir = root.join("installer").join("target").join("release");
        let app_dir = root.join("src-tauri").join("target").join("release");
        fs::create_dir_all(&installer_dir).unwrap();
        fs::create_dir_all(&app_dir).unwrap();
        let exe = write_fake_exe(&app_dir);
        let found = resolve_app_exe(None, &installer_dir, &root).unwrap();
        assert_eq!(found, fs::canonicalize(&exe).unwrap());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resolve_app_exe_finds_repo_root_run() {
        let root = scratch_dir("repo-root");
        let app_dir = root.join("src-tauri").join("target").join("release");
        fs::create_dir_all(&app_dir).unwrap();
        let exe = write_fake_exe(&app_dir);
        let found = resolve_app_exe(None, &root.join("nowhere"), &root).unwrap();
        assert_eq!(found, fs::canonicalize(&exe).unwrap());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resolve_app_exe_errors_when_missing_and_lists_checked_paths() {
        let root = scratch_dir("missing");
        let installer_dir = root.join("installer");
        let cwd = root.join("cwd");
        fs::create_dir_all(&installer_dir).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let err = resolve_app_exe(None, &installer_dir, &cwd).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(APP_EXE), "error names the exe: {msg}");
        assert!(
            msg.contains("cargo tauri build"),
            "error says how to build: {msg}"
        );
        assert!(msg.contains("--from"), "error mentions --from: {msg}");
        assert!(
            msg.contains(&installer_dir.to_string_lossy().to_string()),
            "error lists the installer dir: {msg}"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ensure_config_creates_only_when_missing() {
        let root = scratch_dir("config-create");
        let cfg = root.join("config");
        let outcome = ensure_config(&cfg, "hub_url = \"https://example.invalid\"\n").unwrap();
        assert!(matches!(outcome, ConfigOutcome::Created(_)));
        let path = cfg.join(CONFIG_FILE);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "hub_url = \"https://example.invalid\"\n"
        );
        // Second call keeps the existing file untouched.
        let outcome = ensure_config(&cfg, "hub_url = \"https://other.invalid\"\n").unwrap();
        assert!(matches!(outcome, ConfigOutcome::KeptExisting(_)));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "hub_url = \"https://example.invalid\"\n"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ensure_config_never_overwrites_existing_token() {
        let root = scratch_dir("config-keep");
        let cfg = root.join("config");
        fs::create_dir_all(&cfg).unwrap();
        let path = cfg.join(CONFIG_FILE);
        fs::write(&path, "hub_token = \"REAL-TOKEN-DO-NOT-TOUCH\"\n").unwrap();
        let outcome = ensure_config(&cfg, EMBEDDED_CONFIG_TEMPLATE).unwrap();
        assert!(matches!(outcome, ConfigOutcome::KeptExisting(_)));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "hub_token = \"REAL-TOKEN-DO-NOT-TOUCH\"\n"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn embedded_template_has_placeholder_token_and_hub_url() {
        assert!(
            EMBEDDED_CONFIG_TEMPLATE.contains("hub_token = \"PASTE_YOUR_HUB_TOKEN_HERE\""),
            "embedded template must keep the placeholder token"
        );
        assert!(
            EMBEDDED_CONFIG_TEMPLATE.contains("hub_url = \"https://chat.micro-tech.xyz\""),
            "embedded template must carry the default hub url"
        );
    }

    #[test]
    fn load_config_template_prefers_file_next_to_installer() {
        let root = scratch_dir("template-override");
        let installer_dir = root.join("bin");
        fs::create_dir_all(&installer_dir).unwrap();
        // No file next to the installer -> embedded copy.
        assert_eq!(
            load_config_template(&installer_dir).unwrap(),
            EMBEDDED_CONFIG_TEMPLATE
        );
        // A file next to the installer wins.
        fs::write(
            installer_dir.join(CONFIG_TEMPLATE_FILE),
            "hub_url = \"https://override.invalid\"\n",
        )
        .unwrap();
        assert_eq!(
            load_config_template(&installer_dir).unwrap(),
            "hub_url = \"https://override.invalid\"\n"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn shortcut_ps_script_contains_paths_and_description() {
        let script = shortcut_ps_script(
            Path::new(r"C:\Users\john\Desktop\Muse for Windows.lnk"),
            Path::new(r"C:\Users\john\AppData\Local\muse-windows-app\muse-windows-app.exe"),
            Path::new(r"C:\Users\john\AppData\Local\muse-windows-app"),
            APP_NAME,
        );
        assert!(script.contains("WScript.Shell"), "creates the shell object");
        assert!(script.contains("CreateShortcut"), "creates the shortcut");
        assert!(
            script.contains(r"C:\Users\john\Desktop\Muse for Windows.lnk"),
            "shortcut path: {script}"
        );
        assert!(
            script.contains(r"C:\Users\john\AppData\Local\muse-windows-app\muse-windows-app.exe"),
            "target path: {script}"
        );
        assert!(
            script.contains(r"C:\Users\john\AppData\Local\muse-windows-app"),
            "working dir: {script}"
        );
        assert!(script.contains(APP_NAME), "description: {script}");
        assert!(script.contains("$Shortcut.Save()"), "saves the shortcut");
    }

    #[test]
    fn uninstall_string_quotes_path_and_appends_flag() {
        assert_eq!(
            uninstall_string(Path::new(
                r"C:\Users\john\AppData\Local\muse-windows-app\muse-installer.exe"
            )),
            r#""C:\Users\john\AppData\Local\muse-windows-app\muse-installer.exe" --uninstall"#
        );
    }

    #[test]
    fn uninstall_subkey_is_under_hkcu_path() {
        assert!(
            UNINSTALL_SUBKEY.starts_with(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\"),
            "subkey: {UNINSTALL_SUBKEY}"
        );
        assert!(
            UNINSTALL_SUBKEY.ends_with("Muse for Windows"),
            "subkey: {UNINSTALL_SUBKEY}"
        );
    }

    #[test]
    fn exe_inside_install_dir_detection() {
        let root = scratch_dir("inside-check");
        let install_dir = root.join("muse-windows-app");
        let sibling = root.join("muse-windows-app-2");
        fs::create_dir_all(&install_dir).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let inside = install_dir.join(INSTALLER_EXE);
        let outside = sibling.join(INSTALLER_EXE);
        fs::write(&inside, b"x").unwrap();
        fs::write(&outside, b"x").unwrap();
        assert!(exe_inside_install_dir(&inside, &install_dir));
        assert!(!exe_inside_install_dir(&outside, &install_dir));
        // A sibling with a similar prefix must NOT match (component-wise).
        assert!(!exe_inside_install_dir(&outside, &install_dir));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn same_file_detection() {
        let root = scratch_dir("same-file");
        let a = root.join("a.exe");
        let b = root.join("b.exe");
        fs::write(&a, b"x").unwrap();
        fs::write(&b, b"y").unwrap();
        assert!(same_file(&a, &a));
        assert!(!same_file(&a, &b));
        // Same file via a path with `..` in it (the intermediate dir must
        // exist for canonicalize to resolve it).
        fs::create_dir_all(root.join("sub")).unwrap();
        let via_dotdot = root.join("sub").join("..").join("a.exe");
        assert!(same_file(&a, &via_dotdot));
        fs::remove_dir_all(&root).unwrap();
    }
}

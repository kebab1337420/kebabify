//! Spotify client patcher — modifies the installed Spotify desktop app.
//!
//! Finds the Spotify installation, backs up original files, and applies patches:
//! - Inject custom CSS theme (appended to xpui/user.css)
//! - Inject JS extensions (Spicetify-style, via index.html script tag)
//!
//! Ad removal is handled at runtime by the extension's DOM blocking
//! (`blockAds`) rather than by binary-patching the minified JS bundles, which
//! previously NOP'd bytes and could produce invalid JavaScript.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

/// The patcher instance — holds paths for the Spotify install and user data.
pub struct SpotifyPatcher {
    spotify_dir: PathBuf,
    xpui_dir: PathBuf,
}

impl SpotifyPatcher {
    pub fn new() -> Result<Self> {
        let spotify_dir = find_spotify_install()
            .context("Could not find Spotify installation. Make sure Spotify is installed.")?;
        let xpui_dir = spotify_dir.join("Apps").join("xpui");

        Ok(Self {
            spotify_dir,
            xpui_dir,
        })
    }

    pub fn apply_patches(&self) -> Result<()> {
        self.backup_files()?;
        self.inject_css_theme()?;
        self.inject_js_extensions()?;
        self.sync_spicetify_extension()?;
        Ok(())
    }

    pub fn uninstall_patches(&self) -> Result<()> {
        let backup_dir = self.get_backup_dir();
        if !backup_dir.exists() {
            return Err(anyhow!("No backup found. Nothing to restore."));
        }

        // Restore from backups
        let backup_user_css = backup_dir.join("user.css");
        let user_css_path = self.xpui_dir.join("user.css");
        if backup_user_css.exists() {
            std::fs::copy(&backup_user_css, &user_css_path)
                .with_context(|| format!("Failed to restore user.css from {}", backup_user_css.display()))?;
        } else if user_css_path.exists() {
            // No pristine copy to restore (user.css didn't exist at first
            // apply) — strip the injected block instead of leaving the theme
            // behind after "restore".
            let mut content = std::fs::read_to_string(&user_css_path)
                .with_context(|| format!("Failed to read user.css at {}", user_css_path.display()))?;
            strip_theme_blocks(&mut content);
            std::fs::write(&user_css_path, &content)
                .context("Failed to write restored user.css")?;
        }

        let backup_index = backup_dir.join("index.html");
        let index_path = self.xpui_dir.join("index.html");
        if backup_index.exists() {
            std::fs::copy(&backup_index, &index_path)
                .with_context(|| format!("Failed to restore index.html from {}", backup_index.display()))?;
        }

        // Remove extension files
        for name in ["kebabify_ext.js", "kebaccify_ext.js"] {
            let _ = std::fs::remove_file(self.xpui_dir.join("ext").join(name));
        }

        // The index.html is already restored from backup above, but if the backup
        // contains an old-style kebabify script tag, we need to remove it
        let index_path = self.xpui_dir.join("index.html");
        if index_path.exists() {
            if let Ok(mut content) = std::fs::read_to_string(&index_path) {
                strip_extension_scripts(&mut content);

                let _ = std::fs::write(&index_path, &content);
            }
        }

        eprintln!("  Restored: user.css");
        eprintln!("  Restored: index.html");
        eprintln!("  Removed: kebabify_ext.js");

        self.unsync_spicetify_extension()?;

        Ok(())
    }

    pub fn update_extensions(&self) -> Result<()> {
        // Re-injects the inline script and the theme CSS too — binding only the
        // extension file back would leave user.css stale on theme changes.
        self.inject_css_theme()?;
        self.inject_js_extensions()?;
        self.sync_spicetify_extension()?;
        eprintln!("  Updated: kebabify_theme.css + ext/kebabify_ext.js + index.html inline script");
        Ok(())
    }

    pub fn print_status(&self) -> Result<()> {
        let backup_dir = self.get_backup_dir();
        let backup_exists = backup_dir.exists();

        // Check if kebabify CSS is actually in user.css
        let css_path = self.xpui_dir.join("user.css");
        let css_injected = if css_path.exists() {
            std::fs::read_to_string(&css_path)
                .map(|c| c.contains("kebabify_start") || c.contains("kebaccify_start"))
                .unwrap_or(false)
        } else {
            false
        };

        let js_exists = ["kebabify_ext.js", "kebaccify_ext.js"]
            .iter()
            .any(|name| self.xpui_dir.join("ext").join(name).exists());
        let index_html = self.xpui_dir.join("index.html");
        let index_has_kebab = if index_html.exists() {
            std::fs::read_to_string(&index_html)
                .map(|c| c.contains("kebabify_ext") || c.contains("kebaccify_ext"))
                .unwrap_or(false)
        } else {
            false
        };

        let is_patched = backup_exists && css_injected && index_has_kebab;

        println!("Spotify directory:     {}", self.spotify_dir.display());
        println!("Backup exists:         {}", backup_exists);
        println!("Theme injected:        {}", css_injected);
        println!("Extensions injected:   {}", js_exists && index_has_kebab);
        println!("Status: {}", if is_patched { "Patched" } else { "Not patched" });

        Ok(())
    }

    fn get_backup_dir(&self) -> PathBuf {
        let legacy = self.spotify_dir.join(".kebaccify_backups");
        if legacy.exists() {
            legacy
        } else {
            self.spotify_dir.join(".kebabify_backups")
        }
    }

    fn backup_files(&self) -> Result<()> {
        let backup_dir = self.get_backup_dir();
        std::fs::create_dir_all(&backup_dir)?;

        // Only files that `apply_patches` actually modifies. Because backup
        // files are never overwritten once created, the first run always
        // captures the true originals — re-applying keeps the pristine copy.
        let files_to_backup = vec![
            "user.css",
            "index.html",
        ];

        let mut backed_up = 0;
        for file_name in &files_to_backup {
            let src = self.xpui_dir.join(file_name);
            let dst = backup_dir.join(file_name);
            if src.exists() && !dst.exists() {
                std::fs::copy(&src, &dst)
                    .with_context(|| format!("Failed to backup {}", src.display()))?;
                backed_up += 1;
            }
        }

        eprintln!("  Backed up original files ({})", backed_up);
        Ok(())
    }

    /// Inject kebabify CSS theme into the Spotify user.css file.
    fn inject_css_theme(&self) -> Result<()> {
        let css_path = self.xpui_dir.join("user.css");
        let css = include_str!("kebabify_theme.css");

        let mut content = if css_path.exists() {
            std::fs::read_to_string(&css_path)?
        } else {
            String::new()
        };

        let marker = "/* kebabify_start */";
        let end_marker = "/* kebabify_end */";

        strip_theme_blocks(&mut content);

        // Append kebabify CSS
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&format!(
            "{}\n{}\n{}\n",
            marker, css, end_marker
        ));

        std::fs::write(&css_path, &content)
            .context("Failed to inject CSS theme")?;

        eprintln!("  Injected: user.css (theme applied)");
        Ok(())
    }

    /// Inject JS extension into Spotify's index.html — inlined for reliability.
    fn inject_js_extensions(&self) -> Result<()> {
        let ext_dir = self.xpui_dir.join("ext");
        std::fs::create_dir_all(&ext_dir)?;

        // Write the extension JS file
        let js = include_str!("kebabify_ext.js");
        let ext_path = ext_dir.join("kebabify_ext.js");
        std::fs::write(&ext_path, js).context("Failed to write extension JS")?;
        // Remove the legacy kebaccify file so only one extension exists on disk.
        let _ = std::fs::remove_file(ext_dir.join("kebaccify_ext.js"));

        // Inject INLINE script into index.html (more reliable than external file).
        // Idempotent: any previous kebabify block is removed first.
        let index_path = self.xpui_dir.join("index.html");
        let mut content = std::fs::read_to_string(&index_path)
            .with_context(|| format!("Failed to read index.html at {}", index_path.display()))?;

        strip_extension_scripts(&mut content);

        // Build and insert the inline script.
        let inline_script = format!("<script>\n{}\n</script>\n", js);
        if let Some(pos) = content.find("</body>") {
            content.insert_str(pos, &inline_script);
        } else {
            content.push_str(&inline_script);
        }

        std::fs::write(&index_path, &content)
            .context("Failed to write patched index.html")?;

        eprintln!("  Injected: index.html (JS extension loaded)");
        eprintln!("  Written: ext/kebabify_ext.js");
        Ok(())
    }

    /// Register the extension with Spicetify so its own `apply` keeps loading
    /// our JS. Without this, a `spicetify apply` regenerates index.html and
    /// user.css from its own backup and silently wipes the badge and the rest
    /// of the patch. Returns the spicetify extensions dir when synced.
    fn sync_spicetify_extension(&self) -> Result<Option<PathBuf>> {
        let Some(extensions_dir) = spicetify_extensions_dir() else {
            return Ok(None);
        };

        let js = include_str!("kebabify_ext.js");
        let dst = extensions_dir.join("kebabify_ext.js");
        std::fs::write(&dst, js)
            .with_context(|| format!("Failed to write Spicetify extension at {}", dst.display()))?;

        // Register in config-xpui.ini under [AdditionalOptions] extensions.
        let config_path = extensions_dir.parent().unwrap().join("config-xpui.ini");
        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read Spicetify config at {}", config_path.display()))?;
        let updated = set_config_extensions(&content, "kebabify_ext.js", true);
        if updated != content {
            std::fs::write(&config_path, &updated)
                .with_context(|| format!("Failed to write Spicetify config at {}", config_path.display()))?;
        }

        eprintln!("  Synced: Spicetify extension registered ({})", dst.display());
        Ok(Some(extensions_dir))
    }

    /// Remove the Spicetify registration (extension file + config entry).
    fn unsync_spicetify_extension(&self) -> Result<()> {
        let Some(extensions_dir) = spicetify_extensions_dir() else {
            return Ok(());
        };
        let _ = std::fs::remove_file(extensions_dir.join("kebabify_ext.js"));

        let config_path = extensions_dir.parent().unwrap().join("config-xpui.ini");
        if config_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                let updated = set_config_extensions(&content, "kebabify_ext.js", false);
                if updated != content {
                    let _ = std::fs::write(&config_path, &updated);
                }
            }
        }
        eprintln!("  Removed: Spicetify ext registration");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_both_theme_names_without_removing_user_css() {
        let mut content = String::from("body {}\n/* kebaccify_start */old/* kebaccify_end *//* kebabify_start */new/* kebabify_end */p {}");
        strip_theme_blocks(&mut content);
        assert_eq!(content, "body {}\np {}");
        strip_theme_blocks(&mut content);
        assert_eq!(content, "body {}\np {}");
    }

    #[test]
    fn strips_both_extension_names_and_preserves_other_scripts() {
        let mut content = String::from("<body><script>keep()</script>");
        for name in ["kebaccify", "kebabify"] {
            for quote in ["'", "\""] {
                for defer in ["", "defer "] {
                    content.push_str(&format!("<script {}src={}ext/{}_ext.js{}></script>", defer, quote, name, quote));
                }
            }
            content.push_str(&format!("<script>\n// {}_ext.js\nrun()</script>", name));
        }
        content.push_str("</body>");
        strip_extension_scripts(&mut content);
        assert_eq!(content, "<body><script>keep()</script></body>");
        strip_extension_scripts(&mut content);
        assert_eq!(content, "<body><script>keep()</script></body>");
    }

    #[test]
    fn adds_extension_to_existing_config_ini() {
        let ini = "[AdditionalOptions]\nextensions            = spicetify-jam.js\n";
        let out = set_config_extensions(ini, "kebabify_ext.js", true);
        assert!(out.contains("kebabify_ext.js"));
        assert!(out.contains("spicetify-jam.js"));
    }

    #[test]
    fn adding_extension_to_config_is_idempotent() {
        let ini = "[AdditionalOptions]\nextensions            = spicetify-jam.js\n";
        let once = set_config_extensions(ini, "kebabify_ext.js", true);
        let twice = set_config_extensions(&once, "kebabify_ext.js", true);
        assert_eq!(once, twice);
        assert_eq!(once.matches("kebabify_ext.js").count(), 1);
    }

    #[test]
    fn remove_disables_extension_but_keeps_others() {
        let ini = "[AdditionalOptions]\nextensions            = spicetify-jam.js|kebabify_ext.js\n";
        let out = set_config_extensions(ini, "kebabify_ext.js", false);
        assert!(!out.contains("kebabify_ext.js"));
        assert!(out.contains("spicetify-jam.js"));
    }

    #[test]
    fn adds_extensions_line_when_missing_in_section() {
        let ini = "[AdditionalOptions]\nsidebar_config        = 1\n";
        let out = set_config_extensions(ini, "kebabify_ext.js", true);
        assert!(out.contains("extensions"));
        assert!(out.contains("kebabify_ext.js"));
        assert!(out.contains("sidebar_config"));
    }

    #[test]
    fn strips_spicetify_extensions_script_tag() {
        let mut content = String::from("<body><script defer src='extensions/kebabify_ext.js'></script>keep()</body>");
        strip_extension_scripts(&mut content);
        assert_eq!(content, "<body>keep()</body>");
    }

    #[test]
    fn legacy_backups_take_precedence_without_modification() {
        let root = std::env::temp_dir().join(format!("kebabify_backup_test_{}", std::process::id()));
        let patcher = SpotifyPatcher {
            spotify_dir: root.clone(),
            xpui_dir: root.join("Apps").join("xpui"),
        };
        assert_eq!(patcher.get_backup_dir(), root.join(".kebabify_backups"));
        std::fs::create_dir_all(root.join(".kebabify_backups")).unwrap();
        std::fs::create_dir_all(root.join(".kebaccify_backups")).unwrap();
        assert_eq!(patcher.get_backup_dir(), root.join(".kebaccify_backups"));
        std::fs::remove_dir_all(root).unwrap();
    }
}

fn strip_theme_blocks(content: &mut String) {
    for name in ["kebabify", "kebaccify"] {
        let marker = format!("/* {}_start */", name);
        let end_marker = format!("/* {}_end */", name);
        while let Some(start) = content.find(&marker) {
            let Some(end) = content[start + marker.len()..].find(&end_marker) else {
                break;
            };
            content.drain(start..start + marker.len() + end + end_marker.len());
        }
    }
}

fn strip_extension_scripts(content: &mut String) {
    for name in ["kebabify", "kebaccify"] {
        for dir in ["ext/", "extensions/"] {
            for quote in ["'", "\""] {
                for defer in ["", "defer "] {
                    let tag = format!("<script {}src={}{}{}_ext.js{}></script>", defer, quote, dir, name, quote);
                    *content = content.replace(&tag, "");
                }
            }
        }
        let marker = format!("// {}_ext", name);
        while let Some(start) = content.find(&marker) {
            let Some(script_start) = content[..start].rfind("<script") else {
                break;
            };
            let Some(end) = content[start..].find("</script>") else {
                break;
            };
            content.drain(script_start..start + end + "</script>".len());
        }
    }
}

/// Directory where Spicetify keeps its extensions, when Spicetify is installed.
/// Returns `None` when Spicetify isn't present (typical non-Spicetify setups).
fn spicetify_extensions_dir() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    let dir = PathBuf::from(appdata).join("spicetify");
    dir.join("Extensions").exists().then(|| dir.join("Extensions"))
}

/// Insert (or remove) `name` in the `extensions` list under
/// `[AdditionalOptions]` of a Spicetify `config-xpui.ini`. Preserves any other
/// entries. Returns the possibly-updated full config text.
fn set_config_extensions(ini: &str, name: &str, add: bool) -> String {
    let mut lines: Vec<String> = ini.lines().map(str::to_string).collect();
    let mut in_additional = false;
    let mut found_extensions = false;
    let mut changed = false;

    for line in &mut lines {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_additional = trimmed == "[AdditionalOptions]";
            continue;
        }
        if !in_additional || !trimmed.starts_with("extensions") {
            continue;
        }
        found_extensions = true;
        let Some(eq) = line.find('=') else { continue };
        let key_prefix = line[..=eq].to_string();
        let separator = line[eq + 1..]
            .chars()
            .take_while(|c| c.is_ascii_whitespace())
            .collect::<String>();
        let current: Vec<String> = line[eq + 1 + separator.len()..]
            .split(['|', ','])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let mut next = current.clone();
        if add {
            if !next.iter().any(|v| v == name) {
                next.push(name.to_string());
            }
        } else {
            next.retain(|v| v != name);
        }
        if next != current {
            let value = if next.is_empty() {
                String::new()
            } else {
                next.join("|")
            };
            *line = format!("{}{}{}", key_prefix, separator, value);
            changed = true;
        }
    }

    if add && !found_extensions {
        // No extensions line yet in [AdditionalOptions]: insert one.
        if let Some(pos) = lines.iter().position(|l| l.trim() == "[AdditionalOptions]") {
            lines.insert(pos + 1, "extensions            = ".to_string() + name);
            changed = true;
        }
    }

    if changed {
        lines.join("\n") + "\n"
    } else {
        ini.to_string()
    }
}

/// Locates the Spotify installation directory.
fn find_spotify_install() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        // Modern Spotify installs to APPDATA\Roaming\Spotify
        if let Ok(appdata) = std::env::var("APPDATA") {
            let path = PathBuf::from(appdata).join("Spotify");
            if path.exists() && path.join("Apps\\xpui").exists() {
                return Ok(path);
            }
        }

        // Fallback: LOCALAPPDATA
        if let Ok(localappdata) = std::env::var("LOCALAPPDATA") {
            let path = PathBuf::from(localappdata).join("Spotify");
            if path.exists() && path.join("Apps\\xpui").exists() {
                return Ok(path);
            }
        }

        // Fallback: WindowsApps
        let winapps = PathBuf::from("C:\\Program Files\\WindowsApps\\SpotifyAB.Spotify");
        if winapps.exists() && winapps.join("Apps\\xpui").exists() {
            return Ok(winapps);
        }

        if let Ok(exe) = which::which("spotify") {
            if let Some(parent) = exe.parent() {
                let spotify_path = parent.to_path_buf();
                if spotify_path.join("Apps\\xpui").exists() {
                    return Ok(spotify_path);
                }
            }
        }

        Err(anyhow!("Spotify installation not found. Please install Spotify first."))
    }

    #[cfg(target_os = "macos")]
    {
        let path = PathBuf::from("/Applications/Spotify.app");
        if path.exists() {
            return Ok(path);
        }
        Err(anyhow!("Spotify not found at /Applications/Spotify.app"))
    }

    #[cfg(target_os = "linux")]
    {
        for path in &[
            PathBuf::from("/usr/share/spotify"),
            PathBuf::from("/opt/spotify"),
        ] {
            if path.exists() && path.join("Apps/xpui").exists() {
                return Ok(path.clone());
            }
        }
        Err(anyhow!("Spotify not found. Please install via snap/apt."))
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Err(anyhow!("Unsupported platform"))
    }
}

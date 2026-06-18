use std::path::PathBuf;

/// Manages Holmes profiles — fully isolated data directories.
/// Each profile has its own config.yaml, holmes.db, memory.db, history.txt, output/.
pub struct HolmesProfiles {
    base: PathBuf,
}

impl HolmesProfiles {
    pub fn new() -> Self {
        let base = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Holmes");
        Self { base }
    }

    /// Get the active profile directory, respecting:
    /// 1. --profile CLI arg (via env override)
    /// 2. active_profile file
    /// 3. "default"
    pub fn resolve(&self, cli_profile: Option<&str>) -> PathBuf {
        if let Some(name) = cli_profile {
            return self.profile_dir(name);
        }
        let active_file = self.base.join("active_profile");
        if let Ok(name) = std::fs::read_to_string(&active_file) {
            let name = name.trim();
            if !name.is_empty() {
                return self.profile_dir(name);
            }
        }
        self.profile_dir("default")
    }

    fn profiles_root(&self) -> PathBuf {
        self.base.join("profiles")
    }

    fn profile_dir(&self, name: &str) -> PathBuf {
        self.profiles_root().join(name)
    }

    /// List all profiles
    pub fn list(&self) -> Result<Vec<String>, anyhow::Error> {
        let root = self.profiles_root();
        if !root.exists() {
            return Ok(vec!["default".into()]);
        }
        let mut profiles = Vec::new();
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                profiles.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        if profiles.is_empty() {
            profiles.push("default".into());
        }
        profiles.sort();
        Ok(profiles)
    }

    /// Set the active profile
    pub fn set_active(&self, name: &str) -> Result<(), anyhow::Error> {
        let active_file = self.base.join("active_profile");
        std::fs::create_dir_all(&self.base)?;
        std::fs::write(&active_file, format!("{}\n", name))?;
        Ok(())
    }

    /// Create a new profile
    pub fn create(&self, name: &str, clone_from: Option<&str>) -> Result<(), anyhow::Error> {
        let dir = self.profile_dir(name);
        if dir.exists() {
            anyhow::bail!("profile '{}' already exists", name);
        }
        std::fs::create_dir_all(&dir)?;
        std::fs::create_dir_all(dir.join("output"))?;

        if let Some(source) = clone_from {
            let src_dir = self.profile_dir(source);
            if !src_dir.exists() {
                anyhow::bail!("source profile '{}' does not exist", source);
            }
            let src_config = src_dir.join("config.yaml");
            if src_config.exists() {
                std::fs::copy(&src_config, dir.join("config.yaml"))?;
            }
        }

        println!("Created profile '{}' at {}", name, dir.display());
        Ok(())
    }

    /// Delete a profile
    pub fn delete(&self, name: &str) -> Result<(), anyhow::Error> {
        if name == "default" {
            anyhow::bail!("cannot delete the default profile");
        }
        let dir = self.profile_dir(name);
        if !dir.exists() {
            anyhow::bail!("profile '{}' does not exist", name);
        }
        std::fs::remove_dir_all(&dir)?;

        let active_file = self.base.join("active_profile");
        if let Ok(current) = std::fs::read_to_string(&active_file) {
            if current.trim() == name {
                let _ = std::fs::remove_file(&active_file);
            }
        }
        println!("Deleted profile '{}'", name);
        Ok(())
    }

    /// Show profile info
    pub fn show(&self, name: Option<&str>) -> Result<(), anyhow::Error> {
        let profile_name = name.unwrap_or("default");
        let dir = self.profile_dir(profile_name);
        println!("Profile: {}", profile_name);
        println!("  Directory: {}", dir.display());
        if dir.exists() {
            let config = dir.join("config.yaml");
            println!("  Config: {}", if config.exists() { "✓" } else { "✗ (not configured)" });
            let db = dir.join("holmes.db");
            println!("  Database: {}", if db.exists() { "✓" } else { "✗ (no sessions yet)" });
        } else {
            println!("  (not yet initialized — will be created on first run)");
        }
        Ok(())
    }
}

//! Plugin management and execution (PLUG-001, 005, 007, 008, 010).
//!
//! `marius-plugin-host` owns the JavaScript sandbox; this module owns the
//! lifecycle — install disabled, enable deliberately, grant capability by
//! capability — and the [`HostApi`] implementation that routes every
//! plugin file operation through the same document service the UI and
//! CLI use (PLUG-005), so ARCH-012's source protection applies to
//! plugins exactly as it applies to everything else.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use marius_model::{Capability, PluginInstallation, PluginManifest, VaultId, VaultPath};
use marius_plugin_host::{HitLite, HostApi, PluginHost};
use marius_storage::plugins;

use crate::app::{CoreError, Marius, Result};
use crate::SearchOptions;

impl Marius {
    /// Register a plugin directory (PLUG-001). It arrives disabled with
    /// no capabilities: running it takes `enable_plugin`, and each power
    /// takes its own `grant_plugin_capability`.
    pub fn install_plugin(&mut self, dir: &Path) -> Result<PluginInstallation> {
        let manifest_path = dir.join("manifest.json");
        let manifest_json = std::fs::read_to_string(&manifest_path).map_err(|e| {
            CoreError::Inference(format!("cannot read {}: {e}", manifest_path.display()))
        })?;
        let manifest: PluginManifest = serde_json::from_str(&manifest_json)
            .map_err(|e| CoreError::Inference(format!("manifest: {e}")))?;
        if !dir.join("main.js").exists() {
            return Err(CoreError::Inference(format!(
                "{} has no main.js",
                dir.display()
            )));
        }
        let installation = PluginInstallation::new(
            manifest,
            dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()),
        );
        plugins::upsert(self.db.conn(), &installation)?;
        Ok(installation)
    }

    pub fn plugins(&self) -> Result<Vec<PluginInstallation>> {
        Ok(plugins::list(self.db.conn())?)
    }

    pub fn set_plugin_enabled(&mut self, plugin_id: &str, enabled: bool) -> Result<()> {
        let mut installation = self.plugin(plugin_id)?;
        installation.enabled = enabled;
        plugins::upsert(self.db.conn(), &installation)?;
        Ok(())
    }

    /// Grant or revoke one capability (PLUG-007). The grant is the
    /// user's, recorded one power at a time; nothing is implied.
    pub fn set_plugin_capability(
        &mut self,
        plugin_id: &str,
        capability: Capability,
        granted: bool,
    ) -> Result<()> {
        let mut installation = self.plugin(plugin_id)?;
        installation.granted.retain(|c| *c != capability);
        if granted {
            installation.granted.push(capability);
        }
        plugins::upsert(self.db.conn(), &installation)?;
        Ok(())
    }

    pub fn remove_plugin(&mut self, plugin_id: &str) -> Result<bool> {
        Ok(plugins::remove(self.db.conn(), plugin_id)?)
    }

    fn plugin(&self, plugin_id: &str) -> Result<PluginInstallation> {
        plugins::by_plugin_id(self.db.conn(), plugin_id)?
            .ok_or_else(|| CoreError::Inference(format!("no plugin {plugin_id:?} installed")))
    }
}

/// Routes plugin operations through the document service, scoped to one
/// vault (PLUG-005).
pub struct VaultHostApi {
    app: Rc<RefCell<Marius>>,
    vault: VaultId,
    /// What the plugin printed, surfaced to the caller afterwards.
    pub log: Vec<String>,
}

impl HostApi for VaultHostApi {
    fn search(&mut self, query: &str, limit: usize) -> std::result::Result<Vec<HitLite>, String> {
        let app = self.app.borrow();
        let results = app
            .search(
                query,
                SearchOptions {
                    limit,
                    ..SearchOptions::default()
                },
            )
            .map_err(|e| e.to_string())?;
        Ok(results
            .hits
            .into_iter()
            .filter(|hit| hit.document.vault == self.vault)
            .map(|hit| HitLite {
                path: hit.document.path.to_string(),
                title: hit.document.title,
            })
            .collect())
    }

    fn read(&mut self, path: &str) -> std::result::Result<String, String> {
        let path = VaultPath::new(path).map_err(|e| e.to_string())?;
        let app = self.app.borrow();
        let document = app
            .document_at(self.vault, &path)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no note at {path}"))?;
        app.read(&document).map_err(|e| e.to_string())
    }

    fn write(&mut self, path: &str, text: &str) -> std::result::Result<(), String> {
        let path = VaultPath::new(path).map_err(|e| e.to_string())?;
        let mut app = self.app.borrow_mut();
        let vault = app.vault_by_id(self.vault).map_err(|e| e.to_string())?;
        match app
            .document_at(self.vault, &path)
            .map_err(|e| e.to_string())?
        {
            // save_document enforces every protection: external files,
            // non-editable kinds — and flips the generated-note gate.
            Some(existing) => app
                .save_document(existing.id, text)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            None => app
                .create_document(&vault, &path, text)
                .map(|_| ())
                .map_err(|e| e.to_string()),
        }
    }

    fn log(&mut self, plugin: &str, message: &str) {
        self.log.push(format!("[{plugin}] {message}"));
    }
}

/// Load an enabled plugin and run one of its commands against a vault.
///
/// Free function rather than a method: the plugin may call back into
/// `Marius` mid-command, so the application lives behind `Rc<RefCell>`
/// for the duration and the borrow happens per bridge call.
pub fn run_plugin_command(
    app: Rc<RefCell<Marius>>,
    vault: VaultId,
    plugin_id: &str,
    command: &str,
) -> Result<(String, Vec<String>)> {
    let installation = {
        let borrowed = app.borrow();
        plugins::by_plugin_id(borrowed.db.conn(), plugin_id)?
            .ok_or_else(|| CoreError::Inference(format!("no plugin {plugin_id:?} installed")))?
    };
    if !installation.enabled {
        return Err(CoreError::Inference(format!(
            "plugin {plugin_id:?} is installed but not enabled"
        )));
    }
    let manifest_json = std::fs::read_to_string(installation.install_path.join("manifest.json"))
        .map_err(|e| CoreError::Inference(format!("manifest: {e}")))?;
    let main_js = std::fs::read_to_string(installation.install_path.join("main.js"))
        .map_err(|e| CoreError::Inference(format!("main.js: {e}")))?;

    let api = Rc::new(RefCell::new(VaultHostApi {
        app,
        vault,
        log: Vec::new(),
    }));
    let dyn_api: Rc<RefCell<dyn HostApi>> = api.clone();

    let mut host = PluginHost::load(
        &manifest_json,
        &main_js,
        installation.granted.clone(),
        &dyn_api,
    )
    .map_err(|e| CoreError::Inference(e.to_string()))?;
    let output = host
        .run_command(command, &dyn_api)
        .map_err(|e| CoreError::Inference(e.to_string()))?;
    let log = std::mem::take(&mut api.borrow_mut().log);
    Ok((output, log))
}

/// The commands an installed plugin offers, by loading it read-only.
pub fn plugin_commands(app: Rc<RefCell<Marius>>, plugin_id: &str) -> Result<Vec<String>> {
    let installation = {
        let borrowed = app.borrow();
        plugins::by_plugin_id(borrowed.db.conn(), plugin_id)?
            .ok_or_else(|| CoreError::Inference(format!("no plugin {plugin_id:?} installed")))?
    };
    let manifest_json = std::fs::read_to_string(installation.install_path.join("manifest.json"))
        .map_err(|e| CoreError::Inference(format!("manifest: {e}")))?;
    let main_js = std::fs::read_to_string(installation.install_path.join("main.js"))
        .map_err(|e| CoreError::Inference(format!("main.js: {e}")))?;
    let api = Rc::new(RefCell::new(VaultHostApi {
        app,
        vault: VaultId::nil(),
        log: Vec::new(),
    }));
    let dyn_api: Rc<RefCell<dyn HostApi>> = api;
    // Load with NO capabilities: enumerating commands must not let a
    // disabled-in-spirit plugin touch anything.
    let mut host = PluginHost::load(&manifest_json, &main_js, Vec::new(), &dyn_api)
        .map_err(|e| CoreError::Inference(e.to_string()))?;
    Ok(host.commands())
}

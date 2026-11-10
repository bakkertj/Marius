//! # The plugin runtime (PLUG-002, 005, 007, 008, 010)
//!
//! JavaScript plugins run inside [Boa](https://boajs.dev) — chosen over
//! V8 and QuickJS because it is pure Rust: no C++ toolchain on any
//! platform, which keeps the Windows build as boring as the rest of the
//! workspace (decision recorded in `docs/DECISIONS.md`).
//!
//! The safety story has three layers:
//!
//! 1. **One door.** Plugins reach the application through a single native
//!    function, `__marius_call(json)`. The friendly `marius.*` API is a
//!    JavaScript prelude over it. One door is easy to guard.
//! 2. **Capabilities at the door (PLUG-007).** Every request names an
//!    operation; the dispatcher maps it to a [`Capability`] and refuses
//!    anything the user has not granted. A manifest *request* is never a
//!    grant.
//! 3. **The host never trusts the guest (PLUG-008).** Runtime limits cap
//!    loops, recursion and stack; every evaluation is a `Result`; a
//!    throwing or hanging plugin reports an error and the application
//!    keeps running.
//!
//! The host knows nothing of `marius-core`: it talks to a [`HostApi`]
//! the application implements, which is also what routes plugin file
//! access through the safe document service (PLUG-005) instead of the
//! filesystem.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;

use boa_engine::{js_string, native_function::NativeFunction, Context, JsError, JsValue, Source};
use marius_model::{Capability, PluginManifest};
use serde::{Deserialize, Serialize};

/// What the application lets plugins do. Implemented by `marius-core`
/// over the document service; implemented in tests by an in-memory vault.
pub trait HostApi {
    fn search(&mut self, query: &str, limit: usize) -> Result<Vec<HitLite>, String>;
    fn read(&mut self, path: &str) -> Result<String, String>;
    /// Create or overwrite through the document service — never the raw
    /// filesystem (PLUG-005), so source-protection rules keep applying.
    fn write(&mut self, path: &str, text: &str) -> Result<(), String>;
    fn log(&mut self, plugin: &str, message: &str);
}

/// A search hit as plugins see it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HitLite {
    pub path: String,
    pub title: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("manifest: {0}")]
    Manifest(String),
    #[error("the plugin failed: {0}")]
    Script(String),
    #[error("no command named {0:?}")]
    NoSuchCommand(String),
}

/// The wire format through the one door.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Search { query: String, limit: Option<usize> },
    Read { path: String },
    Write { path: String, text: String },
    Log { message: String },
}

thread_local! {
    /// The dispatcher for the JS call currently on this thread's stack.
    /// Boa's `NativeFunction` cannot safely capture non-GC state, so the
    /// bridge state lives here, installed by a guard around every entry
    /// into the engine and cleared on the way out.
    static BRIDGE: RefCell<Option<Bridge>> = const { RefCell::new(None) };
}

struct Bridge {
    api: Rc<RefCell<dyn HostApi>>,
    granted: Vec<Capability>,
    plugin: String,
}

/// Installs the bridge for the duration of one JS call.
struct BridgeGuard;

impl BridgeGuard {
    fn install(api: Rc<RefCell<dyn HostApi>>, granted: &[Capability], plugin: &str) -> Self {
        BRIDGE.with(|cell| {
            *cell.borrow_mut() = Some(Bridge {
                api,
                granted: granted.to_vec(),
                plugin: plugin.to_owned(),
            });
        });
        Self
    }
}

impl Drop for BridgeGuard {
    fn drop(&mut self) {
        BRIDGE.with(|cell| cell.borrow_mut().take());
    }
}

/// The operation → capability table (PLUG-007). Logging needs nothing;
/// everything else does.
fn required_capability(request: &Request) -> Option<Capability> {
    match request {
        Request::Search { .. } | Request::Read { .. } => Some(Capability::ReadVault),
        Request::Write { .. } => Some(Capability::WriteVault),
        Request::Log { .. } => None,
    }
}

fn dispatch(raw: &str) -> Result<serde_json::Value, String> {
    let request: Request =
        serde_json::from_str(raw).map_err(|e| format!("malformed request: {e}"))?;
    BRIDGE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let bridge = slot.as_mut().ok_or("no host attached")?;
        if let Some(needed) = required_capability(&request) {
            if !bridge.granted.contains(&needed) {
                return Err(format!(
                    "capability {needed} not granted; the user decides, not the manifest"
                ));
            }
        }
        let api = bridge.api.clone();
        let mut api = api.borrow_mut();
        match request {
            Request::Search { query, limit } => {
                let hits = api.search(&query, limit.unwrap_or(20))?;
                Ok(serde_json::to_value(hits).unwrap_or_default())
            }
            Request::Read { path } => {
                let text = api.read(&path)?;
                Ok(serde_json::Value::String(text))
            }
            Request::Write { path, text } => {
                api.write(&path, &text)?;
                Ok(serde_json::Value::Null)
            }
            Request::Log { message } => {
                let plugin = bridge.plugin.clone();
                api.log(&plugin, &message);
                Ok(serde_json::Value::Null)
            }
        }
    })
}

/// The JavaScript half of the API (PLUG-010): a thin, documented surface
/// over the one door. Commands register into a plain object the host can
/// enumerate.
const PRELUDE: &str = r#"
globalThis.__commands = Object.create(null);
globalThis.marius = {
    search(query, limit) {
        return JSON.parse(__marius_call(JSON.stringify({ op: "search", query, limit })));
    },
    read(path) {
        return JSON.parse(__marius_call(JSON.stringify({ op: "read", path })));
    },
    write(path, text) {
        __marius_call(JSON.stringify({ op: "write", path, text }));
    },
    log(message) {
        __marius_call(JSON.stringify({ op: "log", message: String(message) }));
    },
    registerCommand(name, run) {
        if (typeof run !== "function") throw new TypeError("command must be a function");
        __commands[name] = run;
    },
};
"#;

/// One loaded plugin: a Boa context holding its globals and commands.
pub struct PluginHost {
    context: Context,
    manifest: PluginManifest,
    granted: Vec<Capability>,
}

impl PluginHost {
    /// Parse, sandbox and evaluate a plugin. A plugin that throws during
    /// load is an error, not a crash (PLUG-008).
    pub fn load(
        manifest_json: &str,
        main_js: &str,
        granted: Vec<Capability>,
        api: &Rc<RefCell<dyn HostApi>>,
    ) -> Result<Self, PluginError> {
        let manifest: PluginManifest = serde_json::from_str(manifest_json)
            .map_err(|e| PluginError::Manifest(e.to_string()))?;

        let mut context = Context::default();
        let mut limits = context.runtime_limits();
        limits.set_loop_iteration_limit(5_000_000);
        limits.set_recursion_limit(256);
        context.set_runtime_limits(limits);

        context
            .register_global_callable(
                js_string!("__marius_call"),
                1,
                NativeFunction::from_copy_closure(|_this, args, ctx| {
                    let raw = args
                        .first()
                        .cloned()
                        .unwrap_or_default()
                        .to_string(ctx)?
                        .to_std_string_escaped();
                    match dispatch(&raw) {
                        Ok(value) => Ok(JsValue::from(js_string!(value.to_string()))),
                        Err(message) => {
                            Err(JsError::from_opaque(JsValue::from(js_string!(message))))
                        }
                    }
                }),
            )
            .map_err(|e| PluginError::Script(e.to_string()))?;

        let mut host = Self {
            context,
            manifest,
            granted,
        };
        host.eval_with(api, PRELUDE)?;
        let main_js = main_js.to_owned();
        host.eval_with(api, &main_js)?;
        Ok(host)
    }

    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Commands the plugin registered (the palette's source, PLUG-010).
    pub fn commands(&mut self) -> Vec<String> {
        let Ok(value) = self.context.eval(Source::from_bytes(
            "JSON.stringify(Object.keys(__commands))",
        )) else {
            return Vec::new();
        };
        value
            .to_string(&mut self.context)
            .ok()
            .and_then(|s| serde_json::from_str(&s.to_std_string_escaped()).ok())
            .unwrap_or_default()
    }

    /// Run one registered command. Everything the plugin does during the
    /// call goes through the bridge and its capability gate.
    pub fn run_command(
        &mut self,
        name: &str,
        api: &Rc<RefCell<dyn HostApi>>,
    ) -> Result<String, PluginError> {
        if !self.commands().contains(&name.to_owned()) {
            return Err(PluginError::NoSuchCommand(name.to_owned()));
        }
        let call = format!(
            "String(__commands[{}]() ?? \"\")",
            serde_json::Value::String(name.to_owned())
        );
        self.eval_with(api, &call)
    }

    fn eval_with(
        &mut self,
        api: &Rc<RefCell<dyn HostApi>>,
        source: &str,
    ) -> Result<String, PluginError> {
        let _guard = BridgeGuard::install(api.clone(), &self.granted, &self.manifest.id);
        let value = self
            .context
            .eval(Source::from_bytes(source))
            .map_err(|e| PluginError::Script(e.to_string()))?;
        let text = value
            .to_string(&mut self.context)
            .map_err(|e| PluginError::Script(e.to_string()))?
            .to_std_string_escaped();
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A real, minimal vault: a map of path → text, with a log.
    #[derive(Default)]
    struct MemoryVault {
        notes: BTreeMap<String, String>,
        log: Vec<String>,
    }

    impl HostApi for MemoryVault {
        fn search(&mut self, query: &str, limit: usize) -> Result<Vec<HitLite>, String> {
            Ok(self
                .notes
                .iter()
                .filter(|(_, text)| text.contains(query))
                .take(limit)
                .map(|(path, _)| HitLite {
                    path: path.clone(),
                    title: path.trim_end_matches(".md").to_owned(),
                })
                .collect())
        }

        fn read(&mut self, path: &str) -> Result<String, String> {
            self.notes
                .get(path)
                .cloned()
                .ok_or_else(|| format!("no note at {path}"))
        }

        fn write(&mut self, path: &str, text: &str) -> Result<(), String> {
            self.notes.insert(path.to_owned(), text.to_owned());
            Ok(())
        }

        fn log(&mut self, plugin: &str, message: &str) {
            self.log.push(format!("[{plugin}] {message}"));
        }
    }

    const MANIFEST: &str = r#"{ "id": "inbox-count", "name": "Inbox count", "version": "1.0.0" }"#;

    fn vault() -> Rc<RefCell<MemoryVault>> {
        let mut vault = MemoryVault::default();
        vault
            .notes
            .insert("inbox/one.md".into(), "# One\n\nlighthouse business".into());
        vault
            .notes
            .insert("inbox/two.md".into(), "# Two\n\nrecipe".into());
        Rc::new(RefCell::new(vault))
    }

    fn as_api(vault: &Rc<RefCell<MemoryVault>>) -> Rc<RefCell<dyn HostApi>> {
        vault.clone()
    }

    #[test]
    fn a_plugin_reads_searches_and_writes_through_the_door() {
        let vault = vault();
        let api = as_api(&vault);
        let main = r##"
            marius.registerCommand("tally", () => {
                const hits = marius.search("lighthouse");
                const body = "# Tally\n\nFound " + hits.length + " note(s): " +
                    hits.map(h => h.title).join(", ");
                marius.write("tally.md", body);
                marius.log("tally updated");
                return String(hits.length);
            });
        "##;
        let mut host = PluginHost::load(
            MANIFEST,
            main,
            vec![Capability::ReadVault, Capability::WriteVault],
            &api,
        )
        .unwrap();
        assert_eq!(host.commands(), ["tally"]);
        let result = host.run_command("tally", &api).unwrap();
        assert_eq!(result, "1");
        let vault = vault.borrow();
        assert!(vault.notes["tally.md"].contains("Found 1 note(s): inbox/one"));
        assert_eq!(vault.log, ["[inbox-count] tally updated"]);
    }

    #[test]
    fn an_ungranted_capability_is_refused_at_the_door() {
        // PLUG-007: the manifest never grants; the user's list does.
        let vault = vault();
        let api = as_api(&vault);
        let main = r#"
            marius.registerCommand("exfiltrate", () => {
                try {
                    marius.read("inbox/one.md");
                    return "read it";
                } catch (e) {
                    return "refused: " + e;
                }
            });
        "#;
        let mut host = PluginHost::load(MANIFEST, main, vec![], &api).unwrap();
        let result = host.run_command("exfiltrate", &api).unwrap();
        assert!(result.contains("refused"), "{result}");
        assert!(result.contains("read_vault"), "{result}");
    }

    #[test]
    fn a_throwing_plugin_is_an_error_not_a_crash() {
        // PLUG-008, load-time and run-time.
        let vault = vault();
        let api = as_api(&vault);
        let Err(error) =
            PluginHost::load(MANIFEST, "throw new Error('boom at load')", vec![], &api)
        else {
            panic!("a throwing load must fail");
        };
        assert!(error.to_string().contains("boom at load"));

        let main = r#"marius.registerCommand("bad", () => { null.deref; });"#;
        let mut host = PluginHost::load(MANIFEST, main, vec![], &api).unwrap();
        let error = host.run_command("bad", &api).unwrap_err();
        assert!(matches!(error, PluginError::Script(_)));
    }

    #[test]
    fn a_runaway_loop_is_stopped_by_the_runtime_limits() {
        // PLUG-008: a hang is a failure mode too.
        let vault = vault();
        let api = as_api(&vault);
        let main = r#"marius.registerCommand("spin", () => { while (true) {} });"#;
        let mut host = PluginHost::load(MANIFEST, main, vec![], &api).unwrap();
        let error = host.run_command("spin", &api).unwrap_err();
        assert!(matches!(error, PluginError::Script(_)));
    }

    #[test]
    fn unknown_commands_and_bad_manifests_fail_cleanly() {
        let vault = vault();
        let api = as_api(&vault);
        let mut host = PluginHost::load(MANIFEST, "", vec![], &api).unwrap();
        assert!(matches!(
            host.run_command("nope", &api),
            Err(PluginError::NoSuchCommand(_))
        ));
        assert!(matches!(
            PluginHost::load("{ not json", "", vec![], &api),
            Err(PluginError::Manifest(_))
        ));
    }
}

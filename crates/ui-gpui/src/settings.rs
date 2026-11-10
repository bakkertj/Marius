//! The settings view (⌘,): model configuration (AI-001..003, AI-016)
//! and the vault's privacy policy (SEC-005, AI-006/010), without a
//! terminal in sight.
//!
//! Everything here writes through the same `Config` and `VaultPolicy`
//! the CLI uses, so `marius model show` and this pane can never
//! disagree. The credential field stores an environment-variable NAME,
//! never a key (SEC-001) — the pane says so where the user types it.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{div, prelude::*, px, Context, Entity, SharedString, Window};
use marius_core::config::ModelChoice;
use marius_core::Marius;
use marius_model::{InferenceTask, Locality, Sensitivity, VaultId};

use crate::editor::Editor;
use crate::theme::Theme;

/// A chip or button's click handler.
type ClickHandler =
    Box<dyn Fn(&mut SettingsView, &mut Window, &mut Context<SettingsView>) + 'static>;

pub struct SettingsView {
    app: Rc<RefCell<Marius>>,
    vault: VaultId,
    theme: Theme,
    /// Which task's model the form is editing.
    task: InferenceTask,
    provider: Entity<Editor>,
    model: Entity<Editor>,
    base_url: Entity<Editor>,
    credential: Entity<Editor>,
    trusted: bool,
    status: SharedString,
    /// A connection test on its worker thread.
    test: Option<std::sync::mpsc::Receiver<std::result::Result<String, String>>>,
}

impl SettingsView {
    pub fn new(
        app: Rc<RefCell<Marius>>,
        vault: VaultId,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Self {
        let field = |cx: &mut Context<Self>, placeholder: &str| {
            cx.new(|cx| Editor::single_line(cx, theme, placeholder.to_owned()))
        };
        let mut this = Self {
            app,
            vault,
            theme,
            task: InferenceTask::Tagging,
            provider: field(cx, "ollama, openrouter, or any name"),
            model: field(cx, "llama3.2:3b, nomic-embed-text, …"),
            base_url: field(cx, "http://127.0.0.1:11434 (blank = local Ollama default)"),
            credential: field(cx, "ENV VAR NAME holding the key — never the key itself"),
            trusted: false,
            status: "".into(),
            test: None,
        };
        this.load_task(InferenceTask::Tagging, cx);

        // Collect finished connection tests.
        cx.spawn(async move |view, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let Ok(()) = view.update(cx, |view: &mut SettingsView, cx| view.poll_test(cx)) else {
                break;
            };
        })
        .detach();
        this
    }

    pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.theme = theme;
        for editor in [
            &self.provider,
            &self.model,
            &self.base_url,
            &self.credential,
        ] {
            editor.update(cx, |editor, cx| editor.set_theme(theme, cx));
        }
        cx.notify();
    }

    fn load_task(&mut self, task: InferenceTask, cx: &mut Context<Self>) {
        self.task = task;
        let choice = {
            let app = self.app.borrow();
            let models = &app.config().models;
            match task {
                InferenceTask::Tagging => models.tagging.clone(),
                InferenceTask::Embedding => models.embedding.clone(),
                InferenceTask::Summarization => models.summarization.clone(),
                InferenceTask::QuestionAnswering => models.question_answering.clone(),
                InferenceTask::RelationshipDiscovery => models.relationship_discovery.clone(),
            }
        };
        let (provider, model, base_url, credential, trusted) = match choice {
            Some(c) => (
                c.provider,
                c.model,
                c.base_url.unwrap_or_default(),
                c.credential.unwrap_or_default(),
                c.trusted_for_personal,
            ),
            None => Default::default(),
        };
        self.provider.update(cx, |e, cx| e.set_text(&provider, cx));
        self.model.update(cx, |e, cx| e.set_text(&model, cx));
        self.base_url.update(cx, |e, cx| e.set_text(&base_url, cx));
        self.credential
            .update(cx, |e, cx| e.set_text(&credential, cx));
        self.trusted = trusted;
        cx.notify();
    }

    fn form_choice(&self, cx: &Context<Self>) -> Option<ModelChoice> {
        let text = |e: &Entity<Editor>| e.read(cx).text().trim().to_owned();
        let provider = text(&self.provider);
        let model = text(&self.model);
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        let optional = |s: String| (!s.is_empty()).then_some(s);
        Some(ModelChoice {
            provider,
            model,
            credential: optional(text(&self.credential)),
            base_url: optional(text(&self.base_url)),
            locality: None,
            trusted_for_personal: self.trusted,
        })
    }

    fn save_task(&mut self, cx: &mut Context<Self>) {
        let choice = self.form_choice(cx);
        let cleared = choice.is_none();
        let mut app = self.app.borrow_mut();
        let mut config = app.config().clone();
        let slot = match self.task {
            InferenceTask::Tagging => &mut config.models.tagging,
            InferenceTask::Embedding => &mut config.models.embedding,
            InferenceTask::Summarization => &mut config.models.summarization,
            InferenceTask::QuestionAnswering => &mut config.models.question_answering,
            InferenceTask::RelationshipDiscovery => &mut config.models.relationship_discovery,
        };
        *slot = choice;
        match app.set_config(config) {
            Ok(()) => {
                self.status = if cleared {
                    format!("{} cleared (provider and model were blank)", self.task).into()
                } else {
                    format!(
                        "{} saved ({})",
                        self.task,
                        if self.trusted {
                            "trusted with personal notes"
                        } else {
                            "not trusted with personal notes"
                        }
                    )
                    .into()
                };
            }
            Err(error) => self.status = format!("save: {error}").into(),
        }
        cx.notify();
    }

    /// Reach for the configured endpoint on a worker thread: list an
    /// Ollama daemon's models (and say whether the chosen one is
    /// pulled), or sanity-check a remote's URL and credential without
    /// dialling it.
    fn test_connection(&mut self, cx: &mut Context<Self>) {
        if self.test.is_some() {
            return;
        }
        let Some(choice) = self.form_choice(cx) else {
            self.status = "nothing to test: provider and model are blank".into();
            cx.notify();
            return;
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> std::result::Result<String, String> {
                if choice.provider == "ollama" {
                    let base = choice
                        .base_url
                        .clone()
                        .unwrap_or_else(|| marius_llm::ollama::DEFAULT_BASE_URL.to_owned());
                    let daemon = marius_llm::ollama::Ollama::new(base);
                    let models = daemon.models().map_err(|e| e.to_string())?;
                    let present = models.iter().any(|m| {
                        m == &choice.model || m.trim_end_matches(":latest") == choice.model
                    });
                    Ok(if present {
                        format!(
                            "reachable · {} model(s) · '{}' is pulled",
                            models.len(),
                            choice.model
                        )
                    } else {
                        format!(
                            "reachable · {} model(s) · '{}' is NOT pulled (ollama pull {})",
                            models.len(),
                            choice.model,
                            choice.model
                        )
                    })
                } else {
                    let base = choice
                        .base_url
                        .clone()
                        .ok_or("a non-Ollama provider needs a base URL")?;
                    if !base.starts_with("http") {
                        return Err(format!("{base:?} is not a URL"));
                    }
                    let key = match &choice.credential {
                        Some(name) => match std::env::var(name) {
                            Ok(value) => Some(value),
                            Err(_) => {
                                return Err(format!(
                                    "credential variable {name:?} is not set in this \
                                     app's environment (set it, then restart the app)"
                                ))
                            }
                        },
                        None => None,
                    };
                    // GET /models proves DNS, the route, the TLS trust
                    // chain and the credential — no note content moves.
                    let remote = marius_llm::openai::OpenAiCompatible::new(
                        choice.provider.clone(),
                        base,
                        marius_model::Locality::Remote,
                        key,
                    );
                    let models = remote.models().map_err(|e| e.to_string())?;
                    Ok(if models.iter().any(|m| m == &choice.model) {
                        format!(
                            "reachable · {} model(s) · '{}' is offered",
                            models.len(),
                            choice.model
                        )
                    } else {
                        format!(
                            "reachable · {} model(s) · '{}' is not in its listing \
                             (maybe fine: some gateways list partially)",
                            models.len(),
                            choice.model
                        )
                    })
                }
            })();
            let _ = sender.send(result);
        });
        self.test = Some(receiver);
        self.status = "testing…".into();
        cx.notify();
    }

    fn poll_test(&mut self, cx: &mut Context<Self>) {
        let Some(receiver) = &self.test else { return };
        match receiver.try_recv() {
            Ok(Ok(line)) => {
                self.status = line.into();
                self.test = None;
                cx.notify();
            }
            Ok(Err(line)) => {
                self.status = format!("test failed: {line}").into();
                self.test = None;
                cx.notify();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.test = None;
            }
        }
    }

    // ------------------------------------------------------ vault policy

    fn set_sensitivity(&mut self, level: Sensitivity, cx: &mut Context<Self>) {
        let mut app = self.app.borrow_mut();
        match app.vault_policy(self.vault) {
            Ok(mut policy) => {
                policy.default_sensitivity = level;
                match app.set_vault_policy(policy) {
                    Ok(()) => self.status = format!("vault default is now {level}").into(),
                    Err(error) => self.status = format!("policy: {error}").into(),
                }
            }
            Err(error) => self.status = format!("policy: {error}").into(),
        }
        cx.notify();
    }

    fn toggle_auto_classify(&mut self, cx: &mut Context<Self>) {
        let mut app = self.app.borrow_mut();
        if let Ok(mut policy) = app.vault_policy(self.vault) {
            policy.auto_classify = !policy.auto_classify;
            let on = policy.auto_classify;
            if app.set_vault_policy(policy).is_ok() {
                self.status = if on {
                    "auto-classify on: `marius watch` proposes tags for changed notes".into()
                } else {
                    "auto-classify off".into()
                };
            }
        }
        cx.notify();
    }
}

impl gpui::Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let policy = self.app.borrow().vault_policy(self.vault).ok();

        let heading = |label: &str| {
            div()
                .pt(px(18.))
                .pb(px(6.))
                .text_size(px(11.))
                .text_color(theme.muted)
                .child(label.to_uppercase())
        };
        let chip = |id: (&'static str, usize),
                    label: SharedString,
                    active: bool,
                    cx: &mut Context<Self>,
                    on_click: ClickHandler| {
            div()
                .id(id)
                .px(px(10.))
                .py(px(3.))
                .rounded(px(6.))
                .text_size(px(12.))
                .when(active, |d| d.bg(theme.selected).text_color(theme.text))
                .when(!active, |d| d.text_color(theme.muted))
                .hover(|d| d.bg(theme.hover))
                .cursor_pointer()
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
        };
        let row = |label: &'static str, hint: &'static str, editor: Entity<Editor>| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .pb(px(6.))
                .child(
                    div()
                        .w(px(110.))
                        .flex_none()
                        .text_size(px(12.))
                        .text_color(theme.muted)
                        .child(label),
                )
                .child(
                    // The editor paints size_full: without a sized box it
                    // collapses to zero height and cannot be clicked.
                    div()
                        .flex_1()
                        .h(px(30.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(theme.border)
                        .child(editor),
                )
                .child(
                    div()
                        .w(px(230.))
                        .flex_none()
                        .text_size(px(10.))
                        .text_color(theme.muted)
                        .child(hint),
                )
        };
        let button = |id: &'static str,
                      label: &'static str,
                      cx: &mut Context<Self>,
                      on_click: ClickHandler| {
            div()
                .id(id)
                .px(px(12.))
                .py(px(4.))
                .rounded(px(6.))
                .border_1()
                .border_color(theme.border)
                .bg(theme.elevated)
                .text_size(px(12.))
                .hover(|d| d.bg(theme.hover))
                .cursor_pointer()
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
        };

        // Task selector.
        let mut tasks = div().flex().flex_row().gap(px(4.));
        for (index, task) in InferenceTask::ALL.into_iter().enumerate() {
            let active = task == self.task;
            tasks = tasks.child(chip(
                ("settings-task", index),
                task.to_string().replace('_', " ").into(),
                active,
                cx,
                Box::new(move |this, _window, cx| this.load_task(task, cx)),
            ));
        }

        // Locality preview from the current form.
        let locality_note: SharedString = match self.form_choice(cx) {
            Some(choice) => {
                let profile = choice.profile();
                match profile.locality {
                    Locality::Local => "local: every sensitivity may use it".into(),
                    locality => format!(
                        "{locality}: confidential notes never reach it; personal notes only {}",
                        if profile.trusted_for_personal {
                            "because you marked it trusted"
                        } else {
                            "if you mark it trusted"
                        }
                    )
                    .into(),
                }
            }
            None => "blank provider + model = this task stays unconfigured".into(),
        };

        let mut pane = div()
            .id("settings-scroll")
            .size_full()
            .overflow_y_scroll()
            .bg(theme.background)
            .text_color(theme.text)
            .child(
                div()
                    .mx_auto()
                    .w_full()
                    .max_w(px(760.))
                    .px(px(28.))
                    .pb(px(60.))
                    .child(
                        div()
                            .pt(px(16.))
                            .text_size(px(16.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("Settings"),
                    )
                    .child(heading("models — which model answers which task (AI-003)"))
                    .child(tasks)
                    .child(div().h(px(10.)))
                    .child(row(
                        "provider",
                        "ollama is local; anything else is remote",
                        self.provider.clone(),
                    ))
                    .child(row(
                        "model",
                        "the provider's model name, exactly",
                        self.model.clone(),
                    ))
                    .child(row(
                        "base URL",
                        "blank = local Ollama on 11434",
                        self.base_url.clone(),
                    ))
                    .child(row(
                        "credential",
                        "an ENVIRONMENT VARIABLE name — the key itself is never stored (SEC-001)",
                        self.credential.clone(),
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .pt(px(4.))
                            .child(chip(
                                ("settings-trusted", 0),
                                if self.trusted {
                                    "✓ trusted with personal notes".into()
                                } else {
                                    "not trusted with personal notes".into()
                                },
                                self.trusted,
                                cx,
                                Box::new(|this, _w, cx| {
                                    this.trusted = !this.trusted;
                                    // The sensitivity chips below apply
                                    // instantly; this one cannot (it is
                                    // part of the model form), so say so.
                                    this.status = "not applied yet — \
                                                   click 'Save this task'"
                                        .into();
                                    cx.notify();
                                }),
                            ))
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(theme.muted)
                                    .child(locality_note),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .pt(px(12.))
                            .child(button(
                                "settings-save",
                                "Save this task",
                                cx,
                                Box::new(|this, _w, cx| this.save_task(cx)),
                            ))
                            .child(button(
                                "settings-test",
                                "Test connection",
                                cx,
                                Box::new(|this, _w, cx| this.test_connection(cx)),
                            )),
                    ),
            );

        // Vault policy.
        if let Some(policy) = policy {
            let mut levels = div().flex().flex_row().gap(px(4.));
            for (index, level) in Sensitivity::ALL.into_iter().enumerate() {
                levels = levels.child(chip(
                    ("settings-sens", index),
                    level.to_string().into(),
                    policy.default_sensitivity == level,
                    cx,
                    Box::new(move |this, _w, cx| this.set_sensitivity(level, cx)),
                ));
            }
            pane = pane.child(
                div()
                    .mx_auto()
                    .w_full()
                    .max_w(px(760.))
                    .px(px(28.))
                    .child(heading(
                        "vault privacy — the floor every note inherits (SEC-005)",
                    ))
                    .child(levels)
                    .child(
                        div()
                            .pt(px(4.))
                            .text_size(px(10.))
                            .text_color(theme.muted)
                            .child(
                                "personal: remote only via trusted providers · confidential: \
                                 local models only · no_inference: no model at all. A note's \
                                 own setting can tighten this, never loosen it (AI-007).",
                            ),
                    )
                    .child(div().h(px(10.)))
                    .child(chip(
                        ("settings-auto", 0),
                        if policy.auto_classify {
                            "✓ auto-classify changed notes (marius watch)".into()
                        } else {
                            "auto-classify changed notes: off".into()
                        },
                        policy.auto_classify,
                        cx,
                        Box::new(|this, _w, cx| this.toggle_auto_classify(cx)),
                    ))
                    .child(
                        div()
                            .pt(px(14.))
                            .text_size(px(11.))
                            .text_color(theme.accent)
                            .child(self.status.clone()),
                    ),
            );
        }
        pane
    }
}

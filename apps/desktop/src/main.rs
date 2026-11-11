//! The Marius desktop application (UI-001).
//!
//! A thin binary: everything interesting lives in `marius-ui`, and everything
//! *true* lives in `marius-core` — the same services the CLI drives, so the
//! two front ends can never disagree about the data.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Context as _, Result};
use clap::Parser;
use gpui::{
    px, size, App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions,
};
use marius_core::Marius;
use marius_ui::theme::Appearance;
use marius_ui::workspace::Workspace;

#[derive(Parser)]
#[command(name = "marius-desktop", version, about = "Marius — desktop")]
struct Args {
    /// Vault directory to open. Defaults to the configured default vault,
    /// then to the only registered vault.
    vault: Option<PathBuf>,

    /// Where Marius keeps its own state.
    #[arg(long, env = "MARIUS_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Start in light appearance.
    #[arg(long)]
    light: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let mut app = match &args.data_dir {
        Some(dir) => Marius::open(dir)?,
        None => Marius::open_default()?,
    };

    // Resolve which vault to show before any window exists, so a problem is a
    // clear message on stderr rather than an empty frame.
    let vault = match &args.vault {
        Some(path) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Notes".to_owned());
            let vault = app.open_vault(&name, path)?;
            app.index(&vault)?;
            vault
        }
        None => {
            let vaults = app.vaults()?;
            let configured = app.config().default_vault.clone();
            let chosen = match configured {
                Some(name) => vaults.iter().find(|v| v.name.eq_ignore_ascii_case(&name)),
                None => None,
            }
            .or_else(|| vaults.iter().find(|v| v.available))
            .cloned();
            chosen.context(
                "no vault to open; pass a directory, or register one with `marius vault add`",
            )?
        }
    };

    let appearance = if args.light {
        Appearance::Light
    } else {
        Appearance::Dark
    };

    let shared = Rc::new(RefCell::new(app));
    Application::new().run(move |cx: &mut App| {
        let mut bindings = marius_ui::editor::key_bindings();
        bindings.extend(marius_ui::workspace::key_bindings());
        bindings.extend(marius_ui::preview::keybindings());
        cx.bind_keys(bindings);

        let bounds = Bounds::centered(None, size(px(1180.), px(760.)), cx);
        let title = format!("Marius — {}", vault.name);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| Workspace::new(shared.clone(), vault.clone(), appearance, window, cx))
            },
        )
        .expect("open the main window");
        cx.activate(true);
    });
    Ok(())
}

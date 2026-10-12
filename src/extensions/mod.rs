//! Extensions: features that add to crusteo through its extension points
//! rather than being woven into the shell.
//!
//! An extension is Rust compiled into a binary that runs crusteo: the
//! `crusteo` binary runs it with [`builtin`] only, and another binary can
//! pass its own to [`crate::run`]. When the window opens, each extension's
//! [`Extension::contribute`] says what it adds; every part is optional:
//!
//! - toolbar items, and through them bars of their own;
//! - [`Command`]s, with default keys, in the command palette, the help panel
//!   and on macOS the menu bar;
//! - a [`Panel`] beside the layout, opened by one of its commands;
//! - a [`FindSource`] that adds results to find on the Lanes board, and can
//!   answer when Enter is pressed with none chosen;
//! - [`WorkspaceAction`]s in a workspace's header;
//! - a [`NeedsSource`] of items for the "Needs you" strip;
//! - a [`UsageSource`] of numbers for the usage gauges.
//!
//! [`lua`] loads toolbar items from scripts.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::gpui::{AnyElement, AnyView, App, Entity, Global, SharedString, Window};

use crate::app::Core;
use crate::toolbars::ToolbarItem;

pub mod lua;
pub mod needs;
pub mod paseo_usage;
pub mod usage;

/// What extensions get from the shell to build on.
pub struct Host {
    pub core: Core,
    /// The "Needs you" model, which the shell also reads for its palette.
    pub needs: Entity<crate::needs_you::NeedsYou>,
}

pub trait Extension {
    /// What it adds, asked once when the window opens.
    fn contribute(&self, host: &Host, window: &mut Window, cx: &mut App) -> Contributions;
}

/// An extension's parts. Leave out what it does not use.
#[derive(Default)]
pub struct Contributions {
    pub toolbar_items: Vec<Rc<dyn ToolbarItem>>,
    pub commands: Vec<Command>,
    pub panel: Option<Rc<dyn Panel>>,
    pub find: Option<Arc<dyn FindSource>>,
    pub workspace_actions: Vec<WorkspaceAction>,
    pub needs: Option<Rc<dyn NeedsSource>>,
    pub usage: Option<Arc<dyn UsageSource>>,
}

/// Something to do, by name: in the command palette and the help panel,
/// on a key, and on macOS in a menu.
#[derive(Clone)]
pub struct Command {
    /// Unique across extensions, such as `"notes.open"`.
    pub id: SharedString,
    pub name: SharedString,
    /// A default key, in GPUI's form, such as `"secondary-i"` for Ctrl+I
    /// (Cmd+I on macOS).
    pub key: Option<&'static str>,
    /// The macOS menu it goes in, such as `"Help"`; none for none.
    pub menu: Option<&'static str>,
    pub run: Run,
}

/// What a [`Command`] does.
pub type Run = Rc<dyn Fn(&mut Window, &mut App)>;

/// Runs the [`Command`] with this id. Keys and menu items carry it.
#[derive(Clone, PartialEq, Debug, gpui_kit::gpui::Action)]
#[action(namespace = crusteo, no_json)]
pub struct RunCommand {
    pub id: SharedString,
}

/// A view beside the layout, at most one open at a time. Open and close it
/// with [`crate::app::nav::Nav::open_panel`] and
/// [`crate::app::nav::Nav::toggle_panel`], which can hand it some text.
pub trait Panel {
    fn id(&self) -> &str;
    fn view(&self) -> AnyView;
    /// Called when it opens, a frame later, so it can take focus.
    fn focus(&self, window: &mut Window, cx: &mut App);
    /// Whether focus is inside it. The shell takes focus back when it
    /// closes with focus still there.
    fn contains_focus(&self, window: &Window, cx: &App) -> bool;
}

/// An excerpt a [`FindSource`] found, from the conversation of `agent`.
#[derive(Debug, Clone, PartialEq)]
pub struct Excerpt {
    pub agent: String,
    pub text: String,
}

/// What a [`FindSource`] answers when asked.
#[derive(Debug, Clone, PartialEq)]
pub struct FindAnswer {
    pub text: String,
    pub excerpts: Vec<Excerpt>,
}

/// More results for find on the Lanes board, beyond its literal matches.
/// Both calls run off the UI thread, and may be slow.
pub trait FindSource: Send + Sync {
    /// What results from it are marked with, such as `"Notes"`.
    fn name(&self) -> SharedString;
    /// Excerpts that match `query`, asked once typing pauses.
    fn search(&self, query: &str) -> Result<Vec<Excerpt>, String>;
    /// An answer to `query`, asked on Enter with no result chosen. `None`
    /// when this source does not answer questions.
    fn ask(&self, _query: &str) -> Option<Result<FindAnswer, String>> {
        None
    }
}

/// A button in a workspace's header.
#[derive(Clone)]
pub struct WorkspaceAction {
    pub label: SharedString,
    /// An icon from crusteo's set, such as `"brain"`.
    pub icon: Option<&'static str>,
    pub run: RunFor,
}

/// What a [`WorkspaceAction`] does, for its workspace.
pub type RunFor = Rc<dyn Fn(&WorkspaceTarget, &mut Window, &mut App)>;

/// The workspace, and the agent shown in it, a [`WorkspaceAction`] is for.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceTarget {
    pub host: String,
    pub workspace: String,
    pub agent: Option<String>,
    pub agent_title: String,
}

/// An item in the "Needs you" strip.
#[derive(Debug, Clone, PartialEq)]
pub struct NeedsItem {
    pub id: String,
    pub title: String,
    /// What is waiting, after the title.
    pub detail: String,
    /// Shown as chosen, such as while its details are open.
    pub selected: bool,
}

/// Items for the "Needs you" strip, which also shows Paseo's permission
/// prompts. The source keeps itself current, and notifies [`Host::needs`]
/// when its items change.
pub trait NeedsSource {
    /// What its items are, as the palette names them, such as `"Task"`.
    fn kind(&self) -> SharedString;
    /// The icon its items show, from crusteo's set.
    fn icon(&self) -> &'static str {
        "list-checks"
    }
    fn items(&self, cx: &App) -> Vec<NeedsItem>;
    /// A short status at the strip's end, such as when it last heard from
    /// its service, or that it cannot reach it.
    fn status(&self, _cx: &App) -> Option<SharedString> {
        None
    }
    /// The item was clicked, or chosen in the palette.
    fn open(&self, id: &str, window: &mut Window, cx: &mut App);
    /// Something to draw over the window, such as an open item's details.
    fn overlay(&self, _window: &mut Window, _cx: &mut App) -> Option<AnyElement> {
        None
    }
}

/// Where the usage gauges get numbers, such as a quota service. Polled off
/// the UI thread, every [`UsageSource::every`]; the gauges show every
/// source's pools together.
pub trait UsageSource: Send + Sync {
    fn poll(&self) -> Result<crate::app::usage::Reading, String>;
    fn every(&self) -> std::time::Duration {
        std::time::Duration::from_secs(60)
    }
}

/// Every extension's parts, merged. In [`Core::extensions`].
#[derive(Default)]
pub struct Contributed {
    pub commands: Vec<Command>,
    pub panels: Vec<Rc<dyn Panel>>,
    pub find: Vec<Arc<dyn FindSource>>,
    pub workspace_actions: Vec<WorkspaceAction>,
    pub needs: Vec<Rc<dyn NeedsSource>>,
}

impl Contributed {
    pub fn command(&self, id: &str) -> Option<&Command> {
        self.commands.iter().find(|command| command.id == id)
    }
    pub fn panel(&self, id: &str) -> Option<&Rc<dyn Panel>> {
        self.panels.iter().find(|panel| panel.id() == id)
    }
}

/// The extensions [`crate::run`] was given, until the window takes them.
struct Pending(RefCell<Vec<Box<dyn Extension>>>);

impl Global for Pending {}

pub(crate) fn set_pending(extensions: Vec<Box<dyn Extension>>, cx: &mut App) {
    cx.set_global(Pending(RefCell::new(extensions)));
}

pub(crate) fn take_pending(cx: &mut App) -> Vec<Box<dyn Extension>> {
    cx.try_global::<Pending>()
        .map(|pending| pending.0.take())
        .unwrap_or_default()
}

/// The extensions every crusteo has.
pub fn builtin() -> Vec<Box<dyn Extension>> {
    vec![
        Box::new(needs::NeedsStrip),
        Box::new(usage::Usage),
        Box::new(paseo_usage::PaseoUsage),
        Box::new(lua::Scripts),
    ]
}

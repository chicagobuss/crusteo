//! Toolbar items written in Lua.
//!
//! Each `*.lua` file in `<data dir>/toolbars/` is a script. A script
//! declares items with `toolbar.item { ... }`; crusteo runs each item's
//! `update` every `every` seconds and shows what it returns, and calls its
//! `click` when it is clicked. Each script runs in its own Lua state on its
//! own thread, so a slow or broken script cannot stall the window, and a
//! script reloads when its file changes. See docs/toolbars-lua.md.
//!
//! ```lua
//! toolbar.item {
//!   id = "clock", title = "Clock",
//!   bar = "title", side = "right",   -- where it goes until moved
//!   every = 30,                      -- seconds between updates
//!   update = function(fleet)
//!     return { text = os.date("%H:%M"), icon = "clock", tooltip = "Local time" }
//!   end,
//!   click = function() return { toast = "It is " .. os.date("%c") } end,
//! }
//! ```

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::gpui::{
    AnyElement, App, ClickEvent, Hsla, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window, div, px, rgb,
};
use gpui_kit::prelude::FluentBuilder as _;
use mlua::{Function, HookTriggers, Lua, LuaSerdeExt, Table, Value, VmState};
use serde_json::json;

use super::{Contributions, Extension, Host};
use crate::app::Core;
use crate::theme::{palette, size};
use crate::toolbars::{BARS, Side, TITLE, ToolbarItem};
use crate::ui;

/// The scripts in `<data dir>/toolbars/`.
pub struct Scripts;

pub fn scripts_dir() -> PathBuf {
    crate::app::config::data_dir().join("toolbars")
}

impl Extension for Scripts {
    /// Items arrive as their scripts declare them, so none are returned
    /// here; each script's thread reports its items once it has run.
    fn contribute(&self, host: &Host, _: &mut Window, cx: &mut App) -> Contributions {
        let dir = scripts_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Contributions::default();
        };
        let fleet = Arc::new(Mutex::new(json!({})));
        let mut scripts = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("lua") {
                continue;
            }
            start(&host.core, path, fleet.clone(), cx);
            scripts += 1;
        }
        if scripts > 0 {
            share_fleet(&host.core, fleet, cx);
        }
        Contributions::default()
    }
}

/// What an item shows.
#[derive(Clone, Debug, Default, PartialEq)]
struct Shown {
    text: String,
    icon: Option<String>,
    tooltip: Option<String>,
    color: Option<String>,
    error: Option<String>,
}

/// From a script's thread to the window.
enum Event {
    Declared {
        id: String,
        title: String,
        bar: String,
        side: Side,
    },
    Shown {
        id: String,
        shown: Shown,
    },
    /// What a click asked for.
    Act(Act),
}

enum Act {
    Url(String),
    Toast(String),
    Layout(String),
}

/// From the window to a script's thread.
enum Command {
    Click(String),
}

/// Starts a script's thread and the window's side of it.
fn start(core: &Core, path: PathBuf, fleet: Arc<Mutex<serde_json::Value>>, cx: &mut App) {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("script")
        .to_string();
    let (events, events_in) = flume::bounded::<Event>(128);
    let (commands, commands_in) = flume::bounded::<Command>(16);
    let thread_path = path.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("lua {stem}"))
        .spawn(move || run_script(&thread_path, &events, &commands_in, &fleet));
    if let Err(error) = spawned {
        eprintln!("crusteo: lua {}: {error}", path.display());
        return;
    }
    let core = core.clone();
    cx.spawn(async move |cx| {
        let shown: Rc<RefCell<HashMap<String, Shown>>> = Rc::default();
        while let Ok(event) = events_in.recv_async().await {
            cx.update(|cx| match event {
                Event::Declared {
                    id,
                    title,
                    bar,
                    side,
                } => {
                    let item = Rc::new(ScriptItem {
                        id: format!("lua.{stem}.{id}"),
                        script_id: id.clone(),
                        title: title.into(),
                        home: (home_bar(&bar), side),
                        shown: shown.clone(),
                        commands: commands.clone(),
                    });
                    core.toolbars.update(cx, |registry, cx| {
                        registry.register(item);
                        cx.notify();
                    });
                }
                Event::Shown { id, shown: next } => {
                    let changed = shown.borrow().get(&id) != Some(&next);
                    if changed {
                        shown.borrow_mut().insert(id, next);
                        core.toolbars.update(cx, |_, cx| cx.notify());
                    }
                }
                Event::Act(act) => match act {
                    Act::Url(url) => cx.open_url(&url),
                    Act::Toast(text) => core.store.update(cx, |store, cx| store.toast(text, cx)),
                    Act::Layout(layout) => core.nav.update(cx, |nav, cx| {
                        nav.set_layout(&layout, cx);
                    }),
                },
            });
        }
    })
    .detach();
}

/// A bar's id as one of the known bars or a bar of the user's own
/// ("bar-1", made if missing), defaulting to the title bar.
fn home_bar(bar: &str) -> &'static str {
    if let Some(known) = BARS.into_iter().find(|known| *known == bar) {
        return known;
    }
    let own = bar
        .strip_prefix("bar-")
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| n > 0);
    if !own {
        return TITLE;
    }
    // Interned, so reloading a script does not leak another copy.
    static OWN: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let mut own = OWN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(id) = own.iter().find(|id| **id == bar) {
        return id;
    }
    let id: &'static str = Box::leak(bar.to_string().into_boxed_str());
    own.push(id);
    id
}

/// Keeps a summary of the fleet where scripts' `update`s read it.
fn share_fleet(core: &Core, fleet: Arc<Mutex<serde_json::Value>>, cx: &mut App) {
    let core = core.clone();
    cx.spawn(async move |cx| {
        loop {
            let summary = cx.update(|cx| {
                let store = core.store.read(cx);
                let hosts = &store.fleet.hosts;
                // The same counts the title bar shows.
                let hide = !core.nav.read(cx).show_background();
                let counts = store.fleet.agent_bucket_counts(hide);
                let mut agents = serde_json::Map::new();
                for (bucket, count) in counts {
                    agents.insert(
                        bucket.label().to_lowercase().replace(' ', "_"),
                        json!(count),
                    );
                }
                json!({
                    "layout": core.nav.read(cx).layout,
                    "hosts": hosts.len(),
                    "hosts_online": hosts.iter().filter(|h| h.connection.is_online()).count(),
                    "agents": agents,
                })
            });
            if let Ok(mut shared) = fleet.lock() {
                *shared = summary;
            }
            cx.background_executor().timer(Duration::from_secs(2)).await;
        }
    })
    .detach();
}

/// An item a script declared.
struct ScriptItem {
    id: String,
    /// Its id within its script.
    script_id: String,
    title: SharedString,
    home: (&'static str, Side),
    shown: Rc<RefCell<HashMap<String, Shown>>>,
    commands: flume::Sender<Command>,
}

impl ToolbarItem for ScriptItem {
    fn id(&self) -> &str {
        &self.id
    }
    fn title(&self) -> SharedString {
        self.title.clone()
    }
    fn home(&self) -> (&'static str, Side) {
        self.home
    }
    // After the shell's and extensions' items, before the host count.
    fn order(&self) -> i32 {
        50
    }
    fn render(&self, _: &mut Window, _: &mut App) -> Option<AnyElement> {
        let p = palette();
        let shown = self.shown.borrow().get(&self.script_id).cloned()?;
        let color = match &shown.error {
            Some(_) => p.status_danger.into(),
            None => color(shown.color.as_deref()),
        };
        let tooltip = match &shown.error {
            Some(error) => format!("{}: {error}", self.title),
            None => shown
                .tooltip
                .clone()
                .unwrap_or_else(|| self.title.to_string()),
        };
        let (commands, id) = (self.commands.clone(), self.script_id.clone());
        Some(
            div()
                .id(SharedString::from(format!("{}-button", self.id)))
                .h(px(24.))
                .px(px(6.))
                .rounded(px(6.))
                .flex()
                .items_center()
                .gap(px(5.))
                .text_size(size::sm())
                .text_color(color)
                .hover(|style| style.bg(p.sidebar_hover))
                .when_some(shown.icon.clone(), |button, icon| {
                    button.child(ui::icon_named(&icon, 12., color))
                })
                .when(shown.error.is_some(), |button| {
                    button.child(ui::icon("circle-alert", 12., color))
                })
                .when(!shown.text.is_empty(), |button| {
                    button.child(shown.text.clone())
                })
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
                })
                .on_click(move |_: &ClickEvent, _, _| {
                    let _ = commands.try_send(Command::Click(id.clone()));
                })
                .into_any_element(),
        )
    }
}

/// A script's colour: a theme name or `#rrggbb`.
fn color(name: Option<&str>) -> Hsla {
    let p = palette();
    match name {
        Some("accent") => p.accent.into(),
        Some("success") => p.status_success.into(),
        Some("warning") => p.status_warning.into(),
        Some("danger") => p.status_danger.into(),
        Some("foreground") => p.foreground.into(),
        Some(hex) if hex.starts_with('#') && hex.len() == 7 => {
            u32::from_str_radix(&hex[1..], 16).map_or(p.muted.into(), |value| rgb(value).into())
        }
        _ => p.muted.into(),
    }
}

// ---- the script's thread ------------------------------------------------

/// An item as the script declared it.
struct Spec {
    id: String,
    every: Duration,
    update: Option<Function>,
    click: Option<Function>,
    due: Instant,
}

const LUA_MEMORY_LIMIT: usize = 32 * 1024 * 1024;
const LUA_INSTRUCTIONS: usize = 1_000_000;
const HTTP_BODY_LIMIT: u64 = 1024 * 1024;
struct Budget(Cell<usize>);

fn bounded_lua() -> mlua::Result<Lua> {
    let lua = Lua::new();
    lua.set_memory_limit(LUA_MEMORY_LIMIT)?;
    lua.set_app_data(Budget(Cell::new(LUA_INSTRUCTIONS)));
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(10_000),
        |lua, _| {
            let budget = lua
                .app_data_ref::<Budget>()
                .ok_or_else(|| mlua::Error::RuntimeError("missing script budget".into()))?;
            let remaining = budget.0.get();
            if remaining < 10_000 {
                return Err(mlua::Error::RuntimeError(
                    "script instruction limit exceeded".into(),
                ));
            }
            budget.0.set(remaining - 10_000);
            Ok(VmState::Continue)
        },
    );
    Ok(lua)
}

fn reset_budget(lua: &Lua) {
    if let Some(budget) = lua.app_data_ref::<Budget>() {
        budget.0.set(LUA_INSTRUCTIONS);
    }
}

fn interval(seconds: f64) -> mlua::Result<Duration> {
    if !seconds.is_finite() || seconds > 86_400. {
        return Err(mlua::Error::RuntimeError(
            "every must be finite and at most 86400 seconds".into(),
        ));
    }
    Ok(Duration::from_secs_f64(seconds.max(0.5)))
}

fn run_script(
    path: &Path,
    events: &flume::Sender<Event>,
    commands: &flume::Receiver<Command>,
    fleet: &Arc<Mutex<serde_json::Value>>,
) {
    let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    loop {
        let loaded_at: Option<SystemTime> = modified(path);
        let lua = match bounded_lua() {
            Ok(lua) => lua,
            Err(error) => {
                report_load_error(path, &error, events);
                return;
            }
        };
        let mut specs = match load(&lua, path, events) {
            Ok(specs) => specs,
            Err(error) => {
                report_load_error(path, &error, events);
                Vec::new()
            }
        };
        // Run until the file changes, then load it again.
        loop {
            let now = Instant::now();
            for spec in specs.iter_mut().filter(|spec| spec.due <= now) {
                update(&lua, spec, fleet, events);
                spec.due = now + spec.every;
            }
            let next = specs
                .iter()
                .map(|spec| spec.due)
                .min()
                .unwrap_or(now + Duration::from_secs(2));
            let wait = next
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(2));
            match commands.recv_timeout(wait) {
                Ok(Command::Click(id)) => {
                    if let Some(spec) = specs.iter_mut().find(|spec| spec.id == id) {
                        click(&lua, spec, events);
                        update(&lua, spec, fleet, events);
                        spec.due = Instant::now() + spec.every;
                    }
                }
                Err(flume::RecvTimeoutError::Timeout) => {}
                // The window is gone.
                Err(flume::RecvTimeoutError::Disconnected) => return,
            }
            if modified(path) != loaded_at {
                break;
            }
        }
    }
}

/// Runs the script, collecting the items it declares.
fn load(lua: &Lua, path: &Path, events: &flume::Sender<Event>) -> mlua::Result<Vec<Spec>> {
    install_api(lua)?;
    let declared: Rc<RefCell<Vec<Table>>> = Rc::default();
    let collect = declared.clone();
    let toolbar = lua.create_table()?;
    toolbar.set(
        "item",
        lua.create_function(move |_, spec: Table| {
            collect.borrow_mut().push(spec);
            Ok(())
        })?,
    )?;
    lua.globals().set("toolbar", toolbar)?;
    let source = std::fs::read_to_string(path).map_err(mlua::Error::external)?;
    lua.load(source)
        .set_name(
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("script"),
        )
        .exec()?;
    let now = Instant::now();
    let mut specs = Vec::new();
    for spec in declared.borrow().iter() {
        let id: String = spec.get("id")?;
        let title: Option<String> = spec.get("title")?;
        let bar: Option<String> = spec.get("bar")?;
        let side: Option<String> = spec.get("side")?;
        let every: Option<f64> = spec.get("every")?;
        let _ = events.send(Event::Declared {
            id: id.clone(),
            title: title.unwrap_or_else(|| id.clone()),
            bar: bar.unwrap_or_else(|| TITLE.to_string()),
            side: if side.as_deref() == Some("left") {
                Side::Left
            } else {
                Side::Right
            },
        });
        specs.push(Spec {
            id,
            every: interval(every.unwrap_or(60.))?,
            update: spec.get("update")?,
            click: spec.get("click")?,
            due: now,
        });
    }
    Ok(specs)
}

/// `http.get`, `json.decode`, `json.encode` and `log`, on top of Lua's
/// standard library.
fn install_api(lua: &Lua) -> mlua::Result<()> {
    let http = lua.create_table()?;
    http.set(
        "get",
        lua.create_function(|_, url: String| {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(10)))
                .build()
                .into();
            let body = agent.get(&url).call().and_then(|mut response| {
                response
                    .body_mut()
                    .with_config()
                    .limit(HTTP_BODY_LIMIT)
                    .read_to_string()
            });
            Ok(match body {
                Ok(body) => (Some(body), None),
                Err(error) => (None, Some(error.to_string())),
            })
        })?,
    )?;
    lua.globals().set("http", http)?;
    let json_table = lua.create_table()?;
    json_table.set(
        "decode",
        lua.create_function(|lua, text: String| {
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(mlua::Error::external)?;
            lua.to_value(&value)
        })?,
    )?;
    json_table.set(
        "encode",
        lua.create_function(|lua, value: Value| {
            let value: serde_json::Value = lua.from_value(value)?;
            serde_json::to_string(&value).map_err(mlua::Error::external)
        })?,
    )?;
    lua.globals().set("json", json_table)?;
    lua.globals().set(
        "log",
        lua.create_function(|_, text: String| {
            eprintln!("crusteo: lua: {text}");
            Ok(())
        })?,
    )?;
    Ok(())
}

fn update(
    lua: &Lua,
    spec: &Spec,
    fleet: &Arc<Mutex<serde_json::Value>>,
    events: &flume::Sender<Event>,
) {
    let Some(function) = &spec.update else {
        return;
    };
    reset_budget(lua);
    let summary = fleet.lock().map(|fleet| fleet.clone()).unwrap_or_default();
    let shown = lua
        .to_value(&summary)
        .and_then(|summary| function.call::<Value>(summary))
        .map(shown_from)
        .unwrap_or_else(|error| Shown {
            error: Some(first_line(&error.to_string())),
            ..Default::default()
        });
    let _ = events.send(Event::Shown {
        id: spec.id.clone(),
        shown,
    });
}

fn click(lua: &Lua, spec: &Spec, events: &flume::Sender<Event>) {
    let Some(function) = &spec.click else {
        return;
    };
    reset_budget(lua);
    match function.call::<Value>(()) {
        Ok(Value::Table(acts)) => {
            if let Ok(url) = acts.get::<String>("url") {
                let _ = events.send(Event::Act(Act::Url(url)));
            }
            if let Ok(text) = acts.get::<String>("toast") {
                let _ = events.send(Event::Act(Act::Toast(text)));
            }
            if let Ok(layout) = acts.get::<String>("layout") {
                let _ = events.send(Event::Act(Act::Layout(layout)));
            }
        }
        Ok(_) => {}
        Err(error) => {
            let _ = events.send(Event::Act(Act::Toast(format!(
                "{}: {}",
                spec.id,
                first_line(&error.to_string())
            ))));
        }
    }
    let _ = lua;
}

/// What `update` returned: text, or a table of text, icon, tooltip and
/// colour.
fn shown_from(value: Value) -> Shown {
    match value {
        Value::Table(table) => Shown {
            text: table.get::<String>("text").unwrap_or_default(),
            icon: table.get("icon").ok().flatten(),
            tooltip: table.get("tooltip").ok().flatten(),
            color: table.get("color").ok().flatten(),
            error: None,
        },
        Value::Nil => Shown::default(),
        other => Shown {
            text: other.to_string().unwrap_or_default(),
            ..Default::default()
        },
    }
}

/// A script that did not load shows as one item, in the title bar, with
/// the error.
fn report_load_error(path: &Path, error: &mlua::Error, events: &flume::Sender<Event>) {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("script")
        .to_string();
    eprintln!("crusteo: lua {name}: {error}");
    let _ = events.send(Event::Declared {
        id: "error".into(),
        title: name,
        bar: TITLE.into(),
        side: Side::Right,
    });
    let _ = events.send(Event::Shown {
        id: "error".into(),
        shown: Shown {
            error: Some(first_line(&error.to_string())),
            ..Default::default()
        },
    });
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod limits_tests {
    use super::*;

    #[test]
    fn runaway_lua_errors_and_state_remains_usable() {
        let lua = bounded_lua().unwrap();
        assert!(lua.load("while true do end").exec().is_err());
        reset_budget(&lua);
        assert_eq!(lua.load("return 42").eval::<i64>().unwrap(), 42);
        assert!(
            lua.load("return string.rep('x', 64 * 1024 * 1024)")
                .eval::<String>()
                .is_err()
        );
    }

    #[test]
    fn invalid_intervals_do_not_panic_the_worker() {
        assert!(interval(f64::INFINITY).is_err());
        assert!(interval(f64::NAN).is_err());
        assert!(interval(f64::MAX).is_err());
        assert_eq!(interval(0.).unwrap(), Duration::from_millis(500));
        assert_eq!(interval(60.).unwrap(), Duration::from_secs(60));
    }
}

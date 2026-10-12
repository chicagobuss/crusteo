//! Usage meters: each subscription's pace dial, from whatever
//! [`super::UsageSource`]s extensions add. None, no meters.

use std::rc::Rc;

use gpui_kit::gpui::{AnyElement, App, SharedString, Window};

use super::{Contributions, Extension, Host};
use crate::app::Core;
use crate::toolbars::{Side, TITLE, ToolbarItem};

pub struct Usage;

impl Extension for Usage {
    fn contribute(&self, host: &Host, _: &mut Window, _: &mut App) -> Contributions {
        Contributions {
            toolbar_items: vec![Rc::new(Meters {
                core: host.core.clone(),
            })],
            ..Default::default()
        }
    }
}

struct Meters {
    core: Core,
}

impl ToolbarItem for Meters {
    fn id(&self) -> &str {
        "usage-meters"
    }
    fn title(&self) -> SharedString {
        "Usage meters".into()
    }
    fn home(&self) -> (&'static str, Side) {
        (TITLE, Side::Right)
    }
    // The meters give way first, so the host count always stays visible.
    fn shrinks(&self) -> bool {
        true
    }
    fn render(&self, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        Some(crate::fuel::gauges(&self.core, cx))
    }
}

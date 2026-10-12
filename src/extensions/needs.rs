//! The "Needs you" strip: Paseo's permission prompts, and what extensions'
//! [`super::NeedsSource`]s say is waiting on you.

use std::rc::Rc;

use gpui_kit::gpui::{
    AnyElement, AnyView, App, Entity, IntoElement, SharedString, StyleRefinement, Styled, Window,
};

use super::{Contributions, Extension, Host};
use crate::toolbars::{STRIP, Side, ToolbarItem};

pub struct NeedsStrip;

impl Extension for NeedsStrip {
    fn contribute(&self, host: &Host, _: &mut Window, _: &mut App) -> Contributions {
        Contributions {
            toolbar_items: vec![Rc::new(NeedsYouStrip {
                needs: host.needs.clone(),
            })],
            ..Default::default()
        }
    }
}

struct NeedsYouStrip {
    needs: Entity<crate::needs_you::NeedsYou>,
}

impl ToolbarItem for NeedsYouStrip {
    fn id(&self) -> &str {
        "needs-you"
    }
    fn title(&self) -> SharedString {
        "Needs you".into()
    }
    fn home(&self) -> (&'static str, Side) {
        (STRIP, Side::Left)
    }
    fn grows(&self) -> bool {
        true
    }
    fn render(&self, _: &mut Window, _: &mut App) -> Option<AnyElement> {
        Some(
            AnyView::from(self.needs.clone())
                .cached(StyleRefinement::default().size_full())
                .into_any_element(),
        )
    }
}

use gpui::{App, Bounds, IntoElement, ParentElement, Pixels, Styled as _, Window, canvas};

use crate::{TextSelectionGroupId, TextSelectionScopeId};

/// Extends a GPUI parent element with post-layout prepaint observation.
pub trait ElementExt: ParentElement + Sized {
    /// Marks this element subtree as belonging to a text-selection scope.
    fn text_selection_scope(self, scope: TextSelectionScopeId) -> impl IntoElement
    where
        Self: IntoElement,
    {
        crate::text_selection::text_selection_scope(scope, self)
    }

    /// crusteo: marks this subtree's text as one selection group, which a
    /// selection started inside it does not reach past.
    fn text_selection_group(self, group: TextSelectionGroupId) -> impl IntoElement
    where
        Self: IntoElement,
    {
        crate::text_selection::text_selection_group(group, self)
    }

    /// Invokes `callback` during prepaint with this element's resolved bounds.
    fn on_prepaint<F>(self, callback: F) -> Self
    where
        F: FnOnce(Bounds<Pixels>, &mut Window, &mut App) + 'static,
    {
        self.child(
            canvas(
                move |bounds, window, cx| callback(bounds, window, cx),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
    }
}

impl<T: ParentElement> ElementExt for T {}

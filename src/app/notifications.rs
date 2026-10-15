//! Where attention notifications appear: which corner of the window, and
//! whether in the app, in the operating system's notification center, or
//! both.

use gpui_kit::component::notification::NotificationDelivery;
use gpui_kit::gpui::Anchor;

/// The window corner in-app notifications stack in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Corner {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

impl Corner {
    pub const ALL: [Self; 4] = [
        Self::TopLeft,
        Self::TopRight,
        Self::BottomLeft,
        Self::BottomRight,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::TopRight => "top-right",
            Self::TopLeft => "top-left",
            Self::BottomRight => "bottom-right",
            Self::BottomLeft => "bottom-left",
        }
    }

    /// Anything unknown, including nothing saved yet, is the default.
    pub fn parse(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|corner| corner.as_str() == value)
            .unwrap_or_default()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::TopRight => "Top right",
            Self::TopLeft => "Top left",
            Self::BottomRight => "Bottom right",
            Self::BottomLeft => "Bottom left",
        }
    }

    pub fn anchor(self) -> Anchor {
        match self {
            Self::TopRight => Anchor::TopRight,
            Self::TopLeft => Anchor::TopLeft,
            Self::BottomRight => Anchor::BottomRight,
            Self::BottomLeft => Anchor::BottomLeft,
        }
    }
}

/// Where an attention notification is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    InApp,
    System,
    Both,
}

impl Route {
    pub const ALL: [Self; 3] = [Self::InApp, Self::System, Self::Both];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::InApp => "in-app",
            Self::System => "system",
            Self::Both => "both",
        }
    }

    /// Anything unknown, including nothing saved yet, is the default.
    pub fn parse(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|route| route.as_str() == value)
            .unwrap_or_default()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::InApp => "In app",
            Self::System => "System",
            Self::Both => "Both",
        }
    }

    pub fn delivery(self) -> NotificationDelivery {
        match self {
            Self::InApp => NotificationDelivery::InApp,
            Self::System => NotificationDelivery::System,
            Self::Both => NotificationDelivery::InAppAndSystem,
        }
    }
}

/// How in-app notifications sit together when several are up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// A compact pile, fanned out on hover.
    #[default]
    Stack,
    /// Each in full, one under another, as many as fit the window.
    List,
    /// Only the newest; a new one replaces it.
    Newest,
}

impl Layout {
    pub const ALL: [Self; 3] = [Self::Stack, Self::List, Self::Newest];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stack => "stack",
            Self::List => "list",
            Self::Newest => "newest",
        }
    }

    /// Anything unknown, including nothing saved yet, is the default.
    pub fn parse(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|layout| layout.as_str() == value)
            .unwrap_or_default()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Stack => "Stack",
            Self::List => "List",
            Self::Newest => "Newest only",
        }
    }

    fn toast(self) -> gpui_kit::base::ToastLayout {
        match self {
            Self::Stack => gpui_kit::base::ToastLayout::Stack,
            Self::List => gpui_kit::base::ToastLayout::List,
            Self::Newest => gpui_kit::base::ToastLayout::Newest,
        }
    }
}

/// Seconds an agent's notification stays up until changed.
pub const SECONDS_DEFAULT: f32 = 5.;
/// The longest that can be chosen; 0 is until closed.
pub const SECONDS_MAX: f32 = 15.;
/// How many notifications Settings offers to keep at once.
pub const KEEP_CHOICES: [u8; 3] = [3, 5, 10];
pub const KEEP_DEFAULT: u8 = 10;

/// How long an agent's notification stays up, given `seconds` from
/// Settings: None, until closed, for 0.
pub fn timeout(seconds: f32) -> Option<std::time::Duration> {
    (seconds > 0.).then(|| std::time::Duration::from_secs_f32(seconds.min(SECONDS_MAX)))
}

/// Settings' words for `seconds`.
pub fn seconds_label(seconds: f32) -> String {
    if seconds <= 0. {
        "Until closed".into()
    } else if seconds.fract() == 0. {
        format!("{seconds:.0} s")
    } else {
        format!("{seconds:.1} s")
    }
}

/// Tells every notification stack how to lay out and how many to keep.
pub fn apply_layout(layout: Layout, keep: u8) {
    gpui_kit::base::set_toast_preferences(gpui_kit::base::ToastPreferences {
        layout: layout.toast(),
        max: usize::from(keep.max(1)),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_values_round_trip_and_unknown_ones_default() {
        for corner in Corner::ALL {
            assert_eq!(Corner::parse(corner.as_str()), corner);
        }
        for route in Route::ALL {
            assert_eq!(Route::parse(route.as_str()), route);
        }
        for layout in Layout::ALL {
            assert_eq!(Layout::parse(layout.as_str()), layout);
        }
        assert_eq!(Layout::parse(""), Layout::Stack);
        assert_eq!(timeout(0.), None);
        assert_eq!(timeout(2.5), Some(std::time::Duration::from_millis(2500)));
        assert_eq!(seconds_label(0.), "Until closed");
        assert_eq!(seconds_label(5.), "5 s");
        assert_eq!(seconds_label(7.5), "7.5 s");
        assert_eq!(Corner::parse(""), Corner::TopRight);
        assert_eq!(Route::parse("carrier pigeon"), Route::InApp);
    }
}

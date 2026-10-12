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
        assert_eq!(Corner::parse(""), Corner::TopRight);
        assert_eq!(Route::parse("carrier pigeon"), Route::InApp);
    }
}

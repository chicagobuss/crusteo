//! Registered layouts. Adding a way to look at the fleet means adding an
//! entry here; every layout gets the same `Core`.

pub mod board;
pub mod columns;
pub mod fleet;
pub mod lanes;
pub mod paseo;

use crate::app::LayoutEntry;

pub fn registry() -> Vec<LayoutEntry> {
    vec![
        LayoutEntry {
            id: "paseo",
            title: "Paseo",
            build: paseo::build,
            cached: true,
        },
        LayoutEntry {
            id: "fleet",
            title: "Fleet",
            build: fleet::build,
            // Each host column is its own cached view.
            cached: false,
        },
        LayoutEntry {
            id: "board",
            title: "Board",
            build: board::build,
            cached: true,
        },
        LayoutEntry {
            id: "lanes",
            title: "Lanes",
            build: lanes::build,
            // Each column is its own cached view.
            cached: false,
        },
    ]
}

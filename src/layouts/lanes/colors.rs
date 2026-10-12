//! Pure client-local label hues and theme-relative lane surfaces.
use crate::theme::LANE_COLORS;
use gpui_kit::gpui::{Rgba, rgb};
use std::collections::BTreeMap;

pub fn index(key: &str, named: Option<&str>, choices: &BTreeMap<String, String>) -> usize {
    if let Some(index) = choices
        .get(key)
        .and_then(|choice| LANE_COLORS.iter().position(|(name, _)| name == choice))
    {
        return index;
    }
    if let Some(index) = named.and_then(crate::theme::lane_swatch_named) {
        return index;
    }
    crate::theme::paseo_hash(key) as usize % LANE_COLORS.len()
}

/// Mixing with the actual theme surface yields dark tints on dark themes and
/// pale tints on light themes, rather than assigning a fixed background hex.
pub fn tint(surface: Rgba, base: Rgba, amount: f32) -> Rgba {
    let amount = amount.clamp(0., 1.);
    Rgba {
        r: surface.r + (base.r - surface.r) * amount,
        g: surface.g + (base.g - surface.g) * amount,
        b: surface.b + (base.b - surface.b) * amount,
        a: surface.a,
    }
}

pub fn recency(score: f32) -> Rgba {
    let colors = crate::theme::RECENCY_RAMP.map(rgb);
    let scaled = score.clamp(0., 1.) * 3.;
    if scaled >= 3. {
        return colors[3];
    }
    let index = scaled.floor() as usize;
    tint(colors[index], colors[index + 1], scaled - index as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn palette_uses_canonical_keys_and_missing_colors_fall_back() {
        let mut choices = BTreeMap::from([("devtools".into(), "Rose".into())]);
        assert_eq!(
            LANE_COLORS[index("devtools", Some("blue"), &choices)].0,
            "Rose"
        );
        choices.insert("devtools".into(), "Removed color".into());
        assert_eq!(
            LANE_COLORS[index("devtools", Some("blue"), &choices)].0,
            "Azure"
        );
        let synthetic = "\u{0}label-set:[\"a\",\"b\"]";
        choices.insert(synthetic.into(), "Rose".into());
        assert_eq!(LANE_COLORS[index(synthetic, None, &choices)].0, "Rose");
        for (name, swatch) in [
            ("violet", "Violet"),
            ("sky", "Sky"),
            ("emerald", "Mint"),
            ("orange", "Orange"),
            ("pink", "Rose"),
            ("indigo", "Indigo"),
            ("teal", "Teal"),
            ("red", "Brick"),
            ("amber", "Amber"),
            ("blue", "Azure"),
        ] {
            assert_eq!(
                LANE_COLORS[index("workstream", Some(name), &choices)].0,
                swatch
            );
        }
        assert_eq!(
            LANE_COLORS
                .iter()
                .map(|(_, hex)| hex)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            32
        );
    }
    #[test]
    fn tints_follow_theme_brightness_and_ramp_endpoints_are_exact() {
        for (_, color) in LANE_COLORS {
            let dark = tint(rgb(0x1e2120), rgb(color), 0.18);
            let light = tint(rgb(0xf5f5f5), rgb(color), 0.18);
            assert!(dark.r < light.r && dark.g < light.g && dark.b < light.b);
        }
        for (score, hex) in [
            (0., 0x5a9ded),
            (1. / 3., 0x3da975),
            (2. / 3., 0xcba43a),
            (1., 0x9b3b42),
        ] {
            assert_eq!(recency(score), rgb(hex));
        }
        assert_eq!(recency(-1.), rgb(0x5a9ded));
        assert_eq!(recency(2.), rgb(0x9b3b42));
    }
}

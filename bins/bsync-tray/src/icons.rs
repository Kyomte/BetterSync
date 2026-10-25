//! Tray icons drawn in code: a filled circle whose colour shows the state,
//! with a ring, so no image assets are needed.

use crate::status::TrayState;

pub const SIZE: u32 = 32;

pub fn color(state: TrayState) -> [u8; 3] {
    match state {
        TrayState::Idle => [0x2e, 0xa0, 0x43],    // green
        TrayState::Syncing => [0x1f, 0x6f, 0xeb], // blue
        TrayState::Paused => [0x8b, 0x94, 0x9e],  // grey
        TrayState::Error => [0xd1, 0x24, 0x2f],   // red
        TrayState::Offline => [0x57, 0x60, 0x6a], // dark grey
    }
}

/// RGBA pixels, `SIZE`×`SIZE`.
pub fn rgba(state: TrayState) -> Vec<u8> {
    let [r, g, b] = color(state);
    let mut px = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let c = (SIZE as f32 - 1.0) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let outer = c;
            // Anti-aliased edge.
            let alpha = (outer - d + 0.5).clamp(0.0, 1.0);
            let ring = d > outer - 3.0;
            let (pr, pg, pb) = if ring {
                (r / 2, g / 2, b / 2)
            } else {
                (r, g, b)
            };
            px.extend_from_slice(&[pr, pg, pb, (alpha * 255.0) as u8]);
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_distinct_and_transparent_in_corners() {
        let states = [
            TrayState::Idle,
            TrayState::Syncing,
            TrayState::Paused,
            TrayState::Error,
            TrayState::Offline,
        ];
        for s in states {
            let px = rgba(s);
            assert_eq!(px.len(), (SIZE * SIZE * 4) as usize);
            assert_eq!(px[3], 0, "corner is transparent");
            let mid = ((SIZE / 2 * SIZE + SIZE / 2) * 4) as usize;
            assert_eq!(&px[mid..mid + 3], &color(s));
        }
        let colors: std::collections::BTreeSet<_> = states.iter().map(|s| color(*s)).collect();
        assert_eq!(colors.len(), states.len());
    }
}

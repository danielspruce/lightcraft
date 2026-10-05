//! Vendor raw formats (TIFF-based) and helpers shared by them.

pub mod arw;
pub mod cr2;
pub mod nef;
pub mod orf;
pub mod pef;
pub mod raf;
pub mod rw2;

use crate::{BlackLevel, Rect};
use std::ops::Range;

/// Black level per 2×2 CFA position (anchored at the active area origin) from masked sensor columns `cols` over
/// rows `rows`. Uses the median: the crop boundary can include illuminated sensor columns,
/// and hot pixels must not raise the black level. Falls back to 0 when the region is empty.
pub(crate) fn black_from_columns(data: &[u16], width: usize, cols: Range<usize>, rows: Range<usize>, active: Rect) -> BlackLevel {
    let height = data.len() / width.max(1);
    let mut samples: [Vec<u16>; 4] = std::array::from_fn(|_| Vec::new());
    for y in rows.start..rows.end.min(height) {
        let py = (y as isize - active.y as isize).rem_euclid(2) as usize;
        for x in cols.start..cols.end.min(width) {
            let px = (x as isize - active.x as isize).rem_euclid(2) as usize;
            samples[py * 2 + px].push(data[y * width + x]);
        }
    }
    if samples.iter().any(Vec::is_empty) {
        return BlackLevel::uniform(0.0);
    }
    let values = samples
        .iter_mut()
        .map(|v| {
            let mid = v.len() / 2;
            let even = v.len() % 2 == 0;
            let (lower, median, _) = v.select_nth_unstable(mid);
            if even { (*lower.iter().max().unwrap() as f32 + *median as f32) * 0.5 } else { *median as f32 }
        })
        .collect();
    BlackLevel { repeat_rows: 2, repeat_cols: 2, values, delta_h: vec![], delta_v: vec![] }
}

/// White level estimate: the saturation plateau if a noticeable number of samples sit at the maximum value,
/// otherwise the full `bits` range.
pub(crate) fn white_from_data(data: &[u16], bits: u32) -> f32 {
    let full = ((1u32 << bits.clamp(1, 16)) - 1) as f32;
    // samples above the nominal range are padding / invalid (e.g. 0xFFFF fill), not saturation
    let Some(&mx) = data.iter().filter(|&&v| v as f32 <= full).max() else { return full };
    let band = (mx / 256).max(1);
    let near = mx.saturating_sub(band);
    let below = near.saturating_sub(band);
    let step = (data.len() / 2_000_000).max(1);
    let (mut top, mut under) = (0usize, 0usize);
    for &v in data.iter().step_by(step) {
        if v as f32 > full {
            continue;
        }
        if v >= near {
            top += 1;
        } else if v >= below {
            under += 1;
        }
    }
    // a saturation plateau is a spike: many more samples in the top band than in the band just below it
    if mx as f32 > full * 0.5 && top * step >= (data.len() / 20_000).max(16) && top >= 4 * under + 4 {
        // plateau: use its lower edge so everything at saturation maps to ≥ 1.0
        near as f32
    } else {
        full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_columns_by_parity() {
        // 8 wide, masked columns 0..4, active starts at x=4 (even)
        let data: Vec<u16> = (0..8 * 6)
            .map(|i| {
                let (x, y) = (i % 8, i / 8);
                if x < 4 { [100, 101, 102, 103][(y % 2) * 2 + x % 2] } else { 5000 }
            })
            .collect();
        let b = black_from_columns(&data, 8, 0..4, 0..6, Rect::new(4, 0, 4, 6));
        assert_eq!(b.values, vec![100.0, 101.0, 102.0, 103.0]);
        let b = black_from_columns(&data, 8, 0..4, 0..6, Rect::new(3, 1, 4, 5));
        assert_eq!(b.at(0, 0, 0, 1), 103.0);
        assert_eq!(black_from_columns(&data, 8, 0..0, 0..6, Rect::new(0, 0, 8, 6)), BlackLevel::uniform(0.0));
    }

    #[test]
    fn black_columns_ignore_illuminated_crop_border_and_hot_pixels() {
        // A crop border is not necessarily the optical-black boundary. Model a
        // border with 28 dark columns followed by 12 illuminated ones, and an
        // odd crop origin so each CFA plane must also be re-anchored correctly.
        let width = 64;
        let mut data: Vec<u16> = (0..width * 100)
            .map(|i| {
                let (x, y) = (i % width, i / width);
                if x < 28 { [1024, 1026, 1028, 1030][(y % 2) * 2 + x % 2] } else { 4000 }
            })
            .collect();
        data[width * 10 + 4] = 16383;
        let b = black_from_columns(&data, width, 2..38, 3..99, Rect::new(40, 3, 24, 96));
        assert_eq!(b.values, vec![1028.0, 1030.0, 1024.0, 1026.0]);
        // A faint signal survives subtraction instead of being crushed below zero.
        assert_eq!(1050.0 - b.at(0, 0, 0, 1), 22.0);
    }

    #[test]
    fn white_plateau() {
        let mut d: Vec<u16> = (0..100_000).map(|i| (i % 10000) as u16).collect();
        assert_eq!(white_from_data(&d, 14), 16383.0);
        d.extend(std::iter::repeat_n(15000u16, 1000));
        let w = white_from_data(&d, 14);
        assert!((14900.0..=15000.0).contains(&w), "{w}");
        assert_eq!(white_from_data(&[], 12), 4095.0);
    }
}

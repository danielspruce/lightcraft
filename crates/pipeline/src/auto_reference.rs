//! Refine Auto against the camera's embedded rendering. Only ordinary, editable tone and colour
//! controls change; the source remains the RAW. No camera profiles or coefficients are copied.

use std::sync::Arc;

use lightcraft_color::{REC2020, SRGB, perceptual::oklab_from_2020};
use lightcraft_develop::DevelopSettings;
use lightcraft_raster::{
    Rgb32f, Rgba8,
    resample::{Filter, fit, resize},
};

use crate::{RenderRequest, SourceInfo, StageCache, auto::AutoTone, render_cached};

fn labs(img: &Rgba8) -> Vec<[f32; 3]> {
    linear_labs(&img.to_linear())
}

fn linear_labs(img: &Rgb32f) -> Vec<[f32; 3]> {
    let m = REC2020.from_xyz().mul(&SRGB.to_xyz());
    img.data.iter().map(|p| oklab_from_2020(m.apply(p.map(f64::from)).map(|v| v as f32))).collect()
}

fn apply(a: AutoTone, s: &mut DevelopSettings) {
    s.light.exposure = a.exposure;
    s.light.contrast = a.contrast;
    s.light.highlights = a.highlights;
    s.light.shadows = a.shadows;
    s.light.whites = a.whites;
    s.light.blacks = a.blacks;
    s.color.vibrance = a.vibrance;
    s.color.saturation = a.saturation;
    if let Some(calibration) = a.calibration {
        s.calibration = calibration;
    }
}

/// Bounded coordinate search on a small, fully rendered proxy. This measures display colour,
/// rather than assuming scene-linear channel spread predicts the final perceived saturation.
/// Invalid, monochrome or differently cropped previews fall back to the histogram estimate.
/// The camera preview is used only with the as-shot WB and standard colour profile: preserve
/// deliberate creative looks instead of trying to cancel them out.
pub fn refine(src: &Rgb32f, info: &SourceInfo, s: &DevelopSettings, preview: &Rgba8, initial: AutoTone) -> AutoTone {
    if !info.raw
        || s.wb.mode != lightcraft_develop::WbMode::AsShot
        || s.profile.id != "lc.color"
        || s.treatment != lightcraft_develop::Treatment::Color
        || src.width == 0
        || src.height == 0
        || preview.width == 0
        || preview.height == 0
    {
        return initial;
    }
    let aspect = (src.width as f64 / src.height as f64) / (preview.width as f64 / preview.height as f64);
    if !(0.97..=1.03).contains(&aspect) {
        return initial;
    }
    let proxy = Arc::new(fit(src, 96, 96, Filter::Box));
    let mut reference = linear_labs(&resize(&preview.to_linear(), proxy.width, proxy.height, Filter::Box));
    let color = reference.iter().map(|p| p[1].hypot(p[2])).sum::<f32>() / reference.len() as f32;
    if color < 0.008 {
        return initial;
    }
    // Bright camera previews often lift lower tones. Aim their median towards a finished
    // midtone (L=0.67), with a bounded curve that preserves both endpoints. Already dark
    // previews keep their tone intent. This is our rendering preference, not a camera profile.
    let mut lightness: Vec<_> = reference.iter().map(|p| p[0]).collect();
    lightness.sort_by(f32::total_cmp);
    let median = lightness[lightness.len() / 2].clamp(0.01, 0.99);
    let power = (0.67_f32.ln() / median.ln()).clamp(1.0, 1.45);
    for p in &mut reference {
        p[0] = p[0].clamp(0.0, 1.0).powf(power);
    }
    let mut settings = DevelopSettings { wb: s.wb, profile: s.profile.clone(), calibration: s.calibration, ..Default::default() };
    let req = RenderRequest::fit(proxy.width, proxy.height);
    let cache = StageCache::default();
    let mut score = |a: AutoTone| {
        apply(a, &mut settings);
        let rendered = render_cached(&proxy, info, &settings, &req, &cache);
        let actual = labs(&rendered.image);
        actual.iter().zip(&reference).map(|(a, b)| (a[0] - b[0]).powi(2) + 6.0 * ((a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2))).sum::<f32>()
            / actual.len() as f32
    };
    let mut best = AutoTone { calibration: Some(Default::default()), ..initial };
    let mut loss = score(best);
    for scale in [1.0, 0.5, 0.25] {
        for _ in 0..5 {
            let mut improved = false;
            for index in 0..14 {
                for direction in [-1.0, 1.0] {
                    let mut candidate = best;
                    let (value, step, min, max) = match index {
                        0 => (&mut candidate.exposure, 0.5, -4.0, 4.0),
                        1 => (&mut candidate.contrast, 20.0, 0.0, 60.0),
                        2 => (&mut candidate.highlights, 30.0, -90.0, 0.0),
                        3 => (&mut candidate.shadows, 20.0, 0.0, 60.0),
                        4 => (&mut candidate.whites, 20.0, -40.0, 60.0),
                        5 => (&mut candidate.blacks, 15.0, -50.0, 0.0),
                        6 => (&mut candidate.vibrance, 30.0, 0.0, 100.0),
                        7 => (&mut candidate.saturation, 30.0, 0.0, 100.0),
                        i => {
                            let cal = candidate.calibration.as_mut().expect("reference calibration");
                            let v = match i {
                                8 => &mut cal.red_hue,
                                9 => &mut cal.red_sat,
                                10 => &mut cal.green_hue,
                                11 => &mut cal.green_sat,
                                12 => &mut cal.blue_hue,
                                _ => &mut cal.blue_sat,
                            };
                            (v, 20.0, -60.0, 60.0)
                        }
                    };
                    *value = (*value + direction * step * scale).clamp(min, max);
                    if candidate == best {
                        continue;
                    }
                    let next = score(candidate);
                    if next < loss {
                        best = candidate;
                        loss = next;
                        improved = true;
                    }
                }
            }
            if !improved {
                break;
            }
        }
    }
    best.exposure = (best.exposure * 100.0).round() / 100.0;
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auto::auto_tone, render};

    fn scene() -> Rgb32f {
        Rgb32f::from_fn(48, 32, |x, y| {
            let l = 0.03 + x as f32 * 0.006;
            if y < 16 { [l * 0.8, l, l * 1.2] } else { [l * 1.2, l, l * 0.8] }
        })
    }

    #[test]
    fn reference_recovers_muted_raw_and_is_repeatable() {
        let src = scene();
        let info = SourceInfo { raw: true, ..Default::default() };
        let mut target = DevelopSettings::default();
        target.light.exposure = 0.8;
        target.color.saturation = 60.0;
        target.color.vibrance = 40.0;
        let req = RenderRequest::fit(48, 32);
        let preview = render(&src, &info, &target, &req).image;
        let s = DevelopSettings::default();
        let initial = auto_tone(&src, &info, &s);
        let refined = refine(&src, &info, &s, &preview, initial);
        let mut before = s.clone();
        apply(initial, &mut before);
        let mut after = s.clone();
        apply(refined, &mut after);
        let target_lab = labs(&preview);
        let color_error = |settings: &DevelopSettings| {
            labs(&render(&src, &info, settings, &req).image)
                .iter()
                .zip(&target_lab)
                .map(|(a, b)| (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2))
                .sum::<f32>()
        };
        assert!(color_error(&after) < color_error(&before) * 0.5);
        assert!(refined.exposure > initial.exposure);
        assert!(refined.calibration.is_some());
        assert_eq!(refine(&src, &info, &after, &preview, auto_tone(&src, &info, &after)), refined);
    }

    #[test]
    fn missing_colour_wrong_aspect_and_creative_settings_keep_fallback() {
        let src = scene();
        let info = SourceInfo { raw: true, ..Default::default() };
        let mut s = DevelopSettings::default();
        let a = auto_tone(&src, &info, &s);
        let mono = Rgba8::from_fn(48, 32, |_, _| [128, 128, 128, 255]);
        assert_eq!(refine(&src, &info, &s, &mono, a), a);
        let wrong_aspect = Rgba8::from_fn(48, 48, |_, _| [40, 100, 200, 255]);
        assert_eq!(refine(&src, &info, &s, &wrong_aspect, a), a);
        let preview = Rgba8::from_fn(48, 32, |_, _| [40, 100, 200, 255]);
        assert_eq!(refine(&src, &SourceInfo::default(), &s, &preview, a), a);
        s.wb.mode = lightcraft_develop::WbMode::Custom;
        assert_eq!(refine(&src, &info, &s, &preview, a), a);
        s.wb.mode = lightcraft_develop::WbMode::AsShot;
        s.profile.id = "lc.film.warm-print".into();
        assert_eq!(refine(&src, &info, &s, &preview, a), a);
    }
}

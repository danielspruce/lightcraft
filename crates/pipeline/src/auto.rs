//! Auto tone and auto white balance (histogram / grey-world statistics on a proxy).

use lightcraft_color::cct::xy_to_temp_tint;
use lightcraft_color::{REC2020, Xy, bradford, luminance_2020};
use lightcraft_develop::DevelopSettings;
use lightcraft_raster::Rgb32f;
use serde::Serialize;

use crate::SourceInfo;
use crate::local::effective_wb;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct AutoTone {
    pub exposure: f64,
    pub contrast: f64,
    pub highlights: f64,
    pub shadows: f64,
    pub whites: f64,
    pub blacks: f64,
    pub vibrance: f64,
    pub saturation: f64,
    /// Camera-preview-guided primary corrections, when a usable reference is available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration: Option<lightcraft_develop::Calibration>,
}

fn percentile(sorted: &[f32], q: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f32 * q.clamp(0.0, 1.0)) as usize]
}

/// Compute auto tone values for `src` under the current white balance (ignores current tone values).
pub fn auto_tone(src: &Rgb32f, info: &SourceInfo, s: &DevelopSettings) -> AutoTone {
    let mut img = lightcraft_raster::resample::fit(src, 512, 512, lightcraft_raster::resample::Filter::Box);
    let mut base = DevelopSettings { wb: s.wb, ..DevelopSettings::default() };
    base.light.exposure = 0.0;
    crate::local::scene_linear_pre(&mut img, info, &base);
    let mut ev: Vec<f32> = img.data.iter().map(|c| (luminance_2020(*c).max(1e-6) / 0.18).log2()).collect();
    ev.sort_by(|a, b| a.total_cmp(b));
    let median = percentile(&ev, 0.5);
    // A camera-rendered JPEG usually already includes a tone curve and local shadow lift. Raw
    // data does not, so centre its midtones a little more firmly before setting the endpoints.
    let exposure = (if info.raw { -median * 0.95 - 0.12 } else { -median * 0.85 - 0.1 }).clamp(-4.0, 4.0);
    let (p01, p05, p95, p995) =
        (percentile(&ev, 0.01) + exposure, percentile(&ev, 0.05) + exposure, percentile(&ev, 0.95) + exposure, percentile(&ev, 0.995) + exposure);
    let highlights = if p995 > 2.2 { -((p995 - 2.2) * 38.0).min(90.0) } else { 0.0 };
    let shadows = if info.raw {
        // p05 alone misses dark subjects that occupy a modest part of an otherwise bright scene.
        // Use a less extreme tail and lift it enough to reveal detail without moving the black
        // point with it.
        let p10 = percentile(&ev, 0.10) + exposure;
        if p10 < -3.2 { ((-3.2 - p10) * 20.0).min(45.0) } else { 0.0 }
    } else if p05 < -4.0 {
        ((-4.0 - p05) * 22.0).min(70.0)
    } else {
        0.0
    };
    // Judge separation in the body of the histogram. A black background or a few bright
    // lights should not flatten a subject whose midtones are already crowded together.
    let body = percentile(&ev, 0.75) - percentile(&ev, 0.25);
    let spread = p95 - p05;
    let contrast = if info.raw { ((3.5 - body) * 12.0 + 8.0).clamp(0.0, 45.0) } else { ((3.0 - body) * 8.0).clamp(-10.0, 30.0) };
    let whites = if p995 < 1.8 { ((1.8 - p995) * 25.0).min(40.0) } else { -((p995 - 3.5).max(0.0) * 10.0).min(30.0) };
    // Recover shadow detail with Shadows, never by raising the black floor. Lifting both
    // controls turns genuinely black regions grey. Keep a modest toe after shadow recovery.
    let blacks = -((p01 + 5.0).max(0.0) * 10.0 + shadows * 0.2).min(40.0);
    // Adapt the colour boost to midtone chroma; vibrance protects skin and already vivid
    // colours in the colour stage. Ignore deep shadow noise and near-white highlights.
    let gain = exposure.exp2();
    let mut chroma = Vec::new();
    for pixel in &img.data {
        let y = luminance_2020(*pixel) * gain;
        if (0.03..0.75).contains(&y) {
            let hi = pixel.iter().copied().fold(0.0_f32, f32::max);
            let lo = pixel.iter().copied().fold(f32::INFINITY, f32::min);
            chroma.push((hi - lo) / hi.max(1e-6));
        }
    }
    chroma.sort_by(f32::total_cmp);
    let colorfulness = percentile(&chroma, 0.6);
    let color_present = (colorfulness / 0.08).clamp(0.0, 1.0);
    let vibrance = ((0.7 - colorfulness) * 55.0).clamp(0.0, 30.0) * color_present;
    let saturation = ((0.45 - colorfulness) * 12.0).clamp(0.0, 5.0) * color_present;
    // A nearly uniform frame supplies no evidence for aggressive contrast enhancement.
    let contrast = contrast * (spread / 0.75).clamp(0.0, 1.0);
    AutoTone {
        exposure: (exposure as f64 * 100.0).round() / 100.0,
        contrast: contrast.round() as f64,
        highlights: highlights.round() as f64,
        shadows: shadows.round() as f64,
        whites: whites.round() as f64,
        blacks: blacks.round() as f64,
        vibrance: vibrance.round() as f64,
        saturation: saturation.round() as f64,
        calibration: None,
    }
}

/// An automatic black & white mix (slider values, red … magenta) for `src` under `s`'s white
/// balance and tone: each hue band's colourful pixels are pushed away from the image's mean
/// lightness — bands brighter than average get brighter, darker ones darker — so areas that
/// differ only in colour stay apart in grey. Bands with almost no colourful pixels stay at 0.
pub fn auto_bw_mix(src: &Rgb32f, info: &SourceInfo, s: &DevelopSettings) -> [f64; 8] {
    use lightcraft_color::perceptual::{lab_to_lch, oklab_from_2020};
    let mut img = lightcraft_raster::resample::fit(src, 512, 512, lightcraft_raster::resample::Filter::Box);
    let base = DevelopSettings { wb: s.wb, light: s.light, ..DevelopSettings::default() };
    crate::local::scene_linear_pre(&mut img, info, &base);
    let gain = 2f32.powf(base.light.exposure as f32);
    let (mut mass, mut sum_l) = ([0f64; 8], [0f64; 8]);
    let (mut all_l, mut n) = (0f64, 0usize);
    for p in &img.data {
        let lch = lab_to_lch(oklab_from_2020(p.map(|v| (v * gain).max(0.0))));
        all_l += lch[0] as f64;
        n += 1;
        let k = (lch[1] / 0.2).min(1.0) as f64; // as in the B&W conversion
        if k < 0.05 {
            continue;
        }
        let w = crate::colorops::band_weights(lch[2]);
        for i in 0..8 {
            mass[i] += w[i] as f64 * k;
            sum_l[i] += w[i] as f64 * k * lch[0] as f64;
        }
    }
    if n == 0 {
        return [0.0; 8];
    }
    let mean = all_l / n as f64;
    let total: f64 = mass.iter().sum();
    std::array::from_fn(|i| {
        if total <= 0.0 || mass[i] / total < 0.01 {
            return 0.0;
        }
        let sep = sum_l[i] / mass[i] - mean;
        (sep * 400.0).clamp(-60.0, 60.0).round()
    })
}

/// Grey-world white balance weighted towards mid-tone, low-chroma pixels. Returns (temp, tint).
pub fn auto_wb(src: &Rgb32f, info: &SourceInfo) -> (f64, f64) {
    let img = lightcraft_raster::resample::fit(src, 256, 256, lightcraft_raster::resample::Filter::Box);
    let (mut acc, mut wsum) = ([0.0f64; 3], 0.0f64);
    for c in &img.data {
        let y = luminance_2020(*c);
        if !(0.01..=2.0).contains(&y) {
            continue;
        }
        let mx = c[0].max(c[1]).max(c[2]);
        let mn = c[0].min(c[1]).min(c[2]);
        let chroma = (mx - mn) / (mx + 1e-6);
        let w = (1.0 - chroma).powi(2) as f64 * (1.0 - ((y.log2() + 2.5) / 4.0).abs().min(1.0)) as f64;
        for i in 0..3 {
            acc[i] += c[i] as f64 * w;
        }
        wsum += w;
    }
    if wsum <= 0.0 {
        return (info.as_shot_temp, info.as_shot_tint);
    }
    let avg = acc.map(|v| v / wsum);
    let xyz = REC2020.to_xyz().apply(avg);
    let shot = lightcraft_color::cct::temp_tint_to_xy(info.as_shot_temp, info.as_shot_tint);
    let seen = bradford(REC2020.white, shot).apply(xyz);
    let (t, tint) = xy_to_temp_tint(Xy::from_xyz(seen));
    (t.clamp(2000.0, 50000.0).round(), tint.clamp(-150.0, 150.0).round())
}

/// Temperature/tint currently in effect (for UI display).
pub fn current_wb(info: &SourceInfo, s: &DevelopSettings) -> (f64, f64) {
    effective_wb(info, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_background_does_not_flatten_raw_midtones() {
        let img = Rgb32f::from_fn(100, 32, |x, _| {
            let y = match x {
                0..20 => 0.0001,
                20..90 => 0.10 + (x - 20) as f32 * 0.003,
                _ => 0.9,
            };
            [y; 3]
        });
        let info = SourceInfo { raw: true, ..SourceInfo::default() };
        let a = auto_tone(&img, &info, &DevelopSettings::default());
        assert!(a.contrast >= 20.0, "{a:?}");
        assert!(a.shadows > 0.0 && a.shadows <= 45.0, "{a:?}");
        assert!(a.blacks <= 0.0, "{a:?}");
        let base = crate::ToneMap::new(0.0, 0.0, 0.0);
        let edited = crate::ToneMap::new(a.contrast, a.whites, a.blacks);
        let gain = (a.exposure as f32).exp2();
        assert!(edited.apply(0.30 * gain) - edited.apply(0.10 * gain) > base.apply(0.30 * gain) - base.apply(0.10 * gain));
        assert!(edited.apply(0.0001) < base.apply(0.0001));
    }

    #[test]
    fn color_boost_adapts_to_muted_vivid_and_neutral_images() {
        let settings = DevelopSettings::default();
        let info = SourceInfo::default();
        let muted = Rgb32f::from_fn(32, 32, |_, _| [0.22, 0.18, 0.16]);
        let vivid = Rgb32f::from_fn(32, 32, |_, _| [0.6, 0.18, 0.02]);
        let neutral = Rgb32f::from_fn(32, 32, |_, _| [0.18; 3]);
        let m = auto_tone(&muted, &info, &settings);
        let v = auto_tone(&vivid, &info, &settings);
        let n = auto_tone(&neutral, &info, &settings);
        assert!(m.vibrance > v.vibrance && m.saturation > v.saturation);
        assert_eq!((v.vibrance, v.saturation), (0.0, 0.0));
        assert_eq!((n.vibrance, n.saturation, n.contrast), (0.0, 0.0, 0.0));
    }

    #[test]
    fn auto_is_independent_of_previous_auto_adjustments() {
        let img = Rgb32f::from_fn(64, 64, |x, _| [0.01 + x as f32 * 0.005; 3]);
        let info = SourceInfo { raw: true, ..SourceInfo::default() };
        let mut s = DevelopSettings::default();
        let first = auto_tone(&img, &info, &s);
        s.light.exposure = first.exposure;
        s.light.contrast = first.contrast;
        s.light.shadows = first.shadows;
        s.light.highlights = first.highlights;
        s.light.whites = first.whites;
        s.light.blacks = first.blacks;
        s.color.vibrance = first.vibrance;
        s.color.saturation = first.saturation;
        assert_eq!(auto_tone(&img, &info, &s), first);
    }

    #[test]
    fn dark_image_gets_positive_exposure() {
        let img = Rgb32f::from_fn(64, 64, |x, _| [0.01 + x as f32 * 0.0003; 3]);
        let a = auto_tone(&img, &SourceInfo::default(), &DevelopSettings::default());
        assert!(a.exposure > 1.5, "{a:?}");
        let bright = Rgb32f::from_fn(64, 64, |x, _| [0.8 + x as f32 * 0.01; 3]);
        let b = auto_tone(&bright, &SourceInfo::default(), &DevelopSettings::default());
        assert!(b.exposure < -1.0, "{b:?}");
    }

    #[test]
    fn neutral_image_keeps_as_shot_wb() {
        let img = Rgb32f::from_fn(32, 32, |x, y| [0.05 + (x + y) as f32 * 0.004; 3]);
        let (t, tint) = auto_wb(&img, &SourceInfo::default());
        assert!((t - 6500.0).abs() < 150.0, "{t}");
        assert!(tint.abs() < 6.0, "{tint}");
    }

    #[test]
    fn blue_cast_is_corrected_by_higher_temp() {
        // A bluish cast should be neutralised by telling the pipeline the light was bluer (higher K).
        let img = Rgb32f::from_fn(32, 32, |_, _| [0.16, 0.18, 0.24]);
        let (t, _) = auto_wb(&img, &SourceInfo::default());
        assert!(t > 7000.0, "{t}");
        let mut s = DevelopSettings::default();
        s.wb.mode = lightcraft_develop::WbMode::Custom;
        s.wb.temp = t;
        let (t2, tint2) = auto_wb(&img, &SourceInfo::default());
        let _ = (t2, tint2);
        let mut out = img.clone();
        let (tt, ti) = auto_wb(&img, &SourceInfo::default());
        s.wb.temp = tt;
        s.wb.tint = ti;
        crate::local::scene_linear_pre(&mut out, &SourceInfo::default(), &s);
        let c = out.get(0, 0);
        assert!((c[0] - c[2]).abs() < 0.02, "{c:?}");
    }
}

#[cfg(test)]
mod bw_tests {
    use super::*;

    #[test]
    fn auto_bw_mix_pushes_light_and_dark_hues_apart() {
        // left: a light yellow, right: a dark blue (scene-linear Rec. 2020)
        let src = Rgb32f::from_fn(64, 32, |x, _| if x < 32 { [0.55, 0.5, 0.05] } else { [0.01, 0.02, 0.12] });
        let m = auto_bw_mix(&src, &SourceInfo::default(), &DevelopSettings::default());
        let (yellow, blue) = (m[2], m[5]);
        assert!(yellow > 0.0 && blue < 0.0, "{m:?}");
        // an image without colour leaves the mix alone
        let grey = Rgb32f::from_fn(16, 16, |x, _| [x as f32 / 16.0; 3]);
        assert_eq!(auto_bw_mix(&grey, &SourceInfo::default(), &DevelopSettings::default()), [0.0; 8]);
    }
}

//! **Deterministic value noise + fBm** — the worldgen texture function.
//!
//! Pure integer-hash lattice noise: no RNG state, no tables, no floats in the seed path — the same
//! `(x, y, seed)` yields the same value on every machine, every run, forever. That property is
//! load-bearing: the asteroid field is *derived* from this noise on the fly by both the authoritative
//! sim and every renderer (and the live galaxy map mirrors the same function in JS), so all of them see
//! the identical galaxy without ever shipping it over the wire.
//!
//! `fbm2` layers a few octaves of smoothed lattice noise into the classic fractal look — broad belts
//! with clumpy structure inside them — which is what makes the asteroid fields read as *fields*.

/// Deterministic 2-D integer hash → `[0, 1)`. FNV-style mixing, identical to the JS mirror in
/// `galaxymap-web/index.html` (change one, change both).
#[inline]
fn hash01(x: i32, y: i32, seed: u32) -> f32 {
    let mut h = 0x811c9dc5u32 ^ seed.wrapping_mul(0x9e3779b9);
    h = (h ^ (x as u32)).wrapping_mul(0x0100_0193);
    h = (h ^ (y as u32)).wrapping_mul(0x0100_0193);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    (h & 0x00ff_ffff) as f32 / 0x0100_0000 as f32
}

/// Quintic smoothstep — C2-continuous interpolation, no lattice-grid creasing.
#[inline]
fn smooth(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// One octave of value noise at `(x, y)`: bilinear-smooth interpolation of the four surrounding
/// lattice hashes. Returns `[0, 1)`.
pub fn vnoise2(x: f32, y: f32, seed: u32) -> f32 {
    let xf = x.floor();
    let yf = y.floor();
    let (xi, yi) = (xf as i32, yf as i32);
    let (tx, ty) = (smooth(x - xf), smooth(y - yf));
    let a = hash01(xi, yi, seed);
    let b = hash01(xi + 1, yi, seed);
    let c = hash01(xi, yi + 1, seed);
    let d = hash01(xi + 1, yi + 1, seed);
    let top = a + (b - a) * tx;
    let bot = c + (d - c) * tx;
    top + (bot - top) * ty
}

/// Fractal Brownian motion: `octaves` layers of [`vnoise2`], each at double frequency and half
/// amplitude, normalised back to `[0, 1)`.
pub fn fbm2(x: f32, y: f32, octaves: u32, seed: u32) -> f32 {
    let mut sum = 0.0f32;
    let mut amp = 1.0f32;
    let mut norm = 0.0f32;
    let mut fx = x;
    let mut fy = y;
    for o in 0..octaves.max(1) {
        sum += vnoise2(fx, fy, seed.wrapping_add(o * 101)) * amp;
        norm += amp;
        amp *= 0.5;
        fx *= 2.0;
        fy *= 2.0;
    }
    sum / norm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_is_deterministic_and_bounded() {
        for i in -50..50 {
            let (x, y) = (i as f32 * 0.73, i as f32 * -1.19);
            let v1 = fbm2(x, y, 4, 7);
            let v2 = fbm2(x, y, 4, 7);
            assert_eq!(v1, v2, "same input, same value — always");
            assert!((0.0..=1.0).contains(&v1), "bounded: {v1}");
        }
    }

    #[test]
    fn noise_is_smooth_but_not_flat() {
        // Neighbouring samples are close (continuity) while distant regions differ (structure).
        let a = fbm2(10.0, 10.0, 4, 7);
        let b = fbm2(10.05, 10.0, 4, 7);
        assert!((a - b).abs() < 0.12, "continuous: {a} vs {b}");
        let mut lo = 1.0f32;
        let mut hi = 0.0f32;
        for i in 0..400 {
            let v = fbm2(i as f32 * 0.37, i as f32 * 0.61, 4, 7);
            lo = lo.min(v);
            hi = hi.max(v);
        }
        assert!(hi - lo > 0.45, "real dynamic range, not flat: {lo}..{hi}");
    }

    #[test]
    fn different_seeds_yield_different_fields() {
        let mut diff = 0;
        for i in 0..50 {
            let (x, y) = (i as f32 * 0.9, i as f32 * 1.3);
            if (fbm2(x, y, 4, 1) - fbm2(x, y, 4, 2)).abs() > 0.05 {
                diff += 1;
            }
        }
        assert!(diff > 25, "seeds decorrelate the field: {diff}/50");
    }
}

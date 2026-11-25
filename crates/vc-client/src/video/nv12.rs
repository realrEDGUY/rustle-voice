//! Pure-Rust RGBA <-> NV12 conversion (BT.709, limited range), used around the
//! hardware H.264 encoder and decoder.

/// Convert the top-left `w`×`h` of an RGBA image `src_w` pixels wide to NV12
/// (`w` and `h` must be even). Returns `w*h` luma bytes followed by `w*h/2`
/// interleaved U/V bytes.
pub fn rgba_to_nv12(rgba: &[u8], src_w: u32, w: u32, h: u32) -> Vec<u8> {
    debug_assert!(
        w.is_multiple_of(2) && h.is_multiple_of(2),
        "NV12 needs even dimensions"
    );
    let (sw, w, h) = (src_w as usize, w as usize, h as usize);
    let mut out = vec![0u8; w * h * 3 / 2];
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    for y in 0..h {
        let row = &rgba[y * sw * 4..y * sw * 4 + w * 4];
        let dst = &mut y_plane[y * w..(y + 1) * w];
        for (x, d) in dst.iter_mut().enumerate() {
            let p = &row[x * 4..x * 4 + 3];
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            *d = (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8;
        }
    }
    for cy in 0..h / 2 {
        let r0 = &rgba[(2 * cy) * sw * 4..];
        let r1 = &rgba[(2 * cy + 1) * sw * 4..];
        let dst = &mut uv_plane[cy * w..(cy + 1) * w];
        for cx in 0..w / 2 {
            let i = cx * 8;
            let mut sum = [0i32; 3];
            for p in [
                &r0[i..i + 3],
                &r0[i + 4..i + 7],
                &r1[i..i + 3],
                &r1[i + 4..i + 7],
            ] {
                for c in 0..3 {
                    sum[c] += p[c] as i32;
                }
            }
            let (r, g, b) = ((sum[0] + 2) >> 2, (sum[1] + 2) >> 2, (sum[2] + 2) >> 2);
            let u = ((-26 * r - 86 * g + 112 * b + 128) >> 8) + 128;
            let v = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
            dst[cx * 2] = u.clamp(0, 255) as u8;
            dst[cx * 2 + 1] = v.clamp(0, 255) as u8;
        }
    }
    out
}

/// Convert NV12 planes with row pitch `stride` to `w`×`h` RGBA (opaque).
pub fn nv12_to_rgba(y_plane: &[u8], uv_plane: &[u8], stride: usize, w: u32, h: u32) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let yr = &y_plane[y * stride..];
        let uvr = &uv_plane[(y / 2) * stride..];
        let dst = &mut out[y * w * 4..(y + 1) * w * 4];
        for x in 0..w {
            let c = 298 * (yr[x] as i32 - 16);
            let u = uvr[x & !1] as i32 - 128;
            let v = uvr[(x & !1) + 1] as i32 - 128;
            let r = (c + 459 * v + 128) >> 8;
            let g = (c - 55 * u - 136 * v + 128) >> 8;
            let b = (c + 541 * u + 128) >> 8;
            let d = &mut dst[x * 4..x * 4 + 4];
            d[0] = r.clamp(0, 255) as u8;
            d[1] = g.clamp(0, 255) as u8;
            d[2] = b.clamp(0, 255) as u8;
            d[3] = 255;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_roundtrip_within_tolerance() {
        let (w, h) = (64u32, 36u32);
        // Smooth content (chroma is subsampled, so sharp colour edges blur).
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x * 4) as u8, (y * 7) as u8, 200 - (x + y) as u8, 255]);
            }
        }
        let nv12 = rgba_to_nv12(&rgba, w, w, h);
        assert_eq!(nv12.len(), (w * h * 3 / 2) as usize);
        let (yp, uv) = nv12.split_at((w * h) as usize);
        let back = nv12_to_rgba(yp, uv, w as usize, w, h);
        let max_err = rgba
            .iter()
            .zip(&back)
            .map(|(a, b)| (*a as i32 - *b as i32).abs())
            .max()
            .unwrap();
        assert!(max_err <= 12, "max error {max_err}");
        assert!(back.chunks(4).all(|p| p[3] == 255));
    }

    #[test]
    fn nv12_white_black_levels() {
        let white = vec![255u8; 2 * 2 * 4];
        let n = rgba_to_nv12(&white, 2, 2, 2);
        assert_eq!(&n[..4], &[235; 4]);
        assert_eq!(&n[4..], &[128, 128]);
        let black: Vec<u8> = [0, 0, 0, 255].repeat(4);
        let n = rgba_to_nv12(&black, 2, 2, 2);
        assert_eq!(&n[..4], &[16; 4]);
    }

    #[test]
    fn nv12_crops_from_wider_source() {
        // A 4x2 source cropped to 2x2: only the left half is read.
        let mut rgba = [255u8, 255, 255, 255].repeat(2);
        rgba.extend([0u8, 0, 0, 255].repeat(2));
        let row = rgba.clone();
        rgba.extend(row);
        let n = rgba_to_nv12(&rgba, 4, 2, 2);
        assert_eq!(&n[..4], &[235; 4]);
    }
}

//! Checked conversion for MediaCodec's linear 8-bit YUV420 output.

pub(super) struct YuvLayout {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub slice_height: usize,
    pub left: usize,
    pub top: usize,
    pub planar: bool,
}

pub(super) fn to_rgb(data: &[u8], layout: YuvLayout) -> Result<Vec<u8>, &'static str> {
    let YuvLayout {
        width,
        height,
        stride,
        slice_height,
        left,
        top,
        planar,
    } = layout;
    if width
        .checked_mul(height)
        .is_none_or(|pixels| pixels > 4096 * 4096)
    {
        return Err("YUV frame exceeds pixel budget");
    }
    if width == 0
        || height == 0
        || stride > 8192
        || slice_height > 8192
        || left.checked_add(width).is_none_or(|n| n > stride)
        || top.checked_add(height).is_none_or(|n| n > slice_height)
    {
        return Err("invalid YUV dimensions or crop");
    }
    let y_size = stride * slice_height;
    let chroma_stride = stride.div_ceil(2);
    let chroma_size = chroma_stride * slice_height.div_ceil(2);
    // NV12 stores interleaved UV with an even byte stride.
    let uv_stride = chroma_stride * 2;
    if data.len() < y_size + chroma_size * 2 {
        return Err("truncated YUV output buffer");
    }
    let mut rgb = Vec::with_capacity(width * height * 3);
    for row in top..top + height {
        for col in left..left + width {
            let y = i32::from(data[row * stride + col]) - 16;
            let chroma = (row / 2) * chroma_stride + col / 2;
            let (u, v) = if planar {
                (data[y_size + chroma], data[y_size + chroma_size + chroma])
            } else {
                let i = y_size + (row / 2) * uv_stride + (col / 2) * 2;
                (data[i], data[i + 1])
            };
            let u = i32::from(u) - 128;
            let v = i32::from(v) - 128;
            // BT.601 limited range. Other color standards are rejected by caller.
            for value in [
                298 * y + 409 * v,
                298 * y - 100 * u - 208 * v,
                298 * y + 516 * u,
            ] {
                rgb.push(((value + 128) >> 8).clamp(0, 255) as u8);
            }
        }
    }
    Ok(rgb)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn layout(planar: bool) -> YuvLayout {
        YuvLayout {
            width: 2,
            height: 2,
            stride: 2,
            slice_height: 2,
            left: 0,
            top: 0,
            planar,
        }
    }
    #[test]
    fn planar_and_interleaved_black_and_white() {
        for planar in [true, false] {
            assert_eq!(
                to_rgb(&[16, 235, 16, 235, 128, 128], layout(planar)).unwrap(),
                [0, 0, 0, 255, 255, 255, 0, 0, 0, 255, 255, 255]
            );
        }
    }
    #[test]
    fn padded_crop_and_chroma_planes() {
        let mut data = vec![16; 24];
        data[5] = 235;
        data[16..].fill(128);
        let mut crop = layout(true);
        crop.stride = 4;
        crop.slice_height = 4;
        crop.left = 1;
        crop.top = 1;
        crop.width = 1;
        crop.height = 1;
        assert_eq!(to_rgb(&data, crop).unwrap(), [255, 255, 255]);
        // Red: distinguish UV order, not just neutral chroma.
        assert_eq!(
            to_rgb(&[81, 81, 81, 81, 90, 240], layout(false)).unwrap()[..3],
            [255, 0, 0]
        );
    }
    #[test]
    fn invalid_buffers_are_rejected_before_indexing() {
        assert!(to_rgb(&[0; 5], layout(true)).is_err());
        let mut bad = layout(false);
        bad.left = usize::MAX;
        assert!(to_rgb(&[], bad).is_err());
        let mut bad = layout(true);
        bad.stride = usize::MAX;
        assert!(to_rgb(&[], bad).is_err());
    }
}

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

const MAX_AVATAR_EDGE: u32 = 256;

/// Raster masks work with software rendering too, where rounded child clipping
/// is not implemented. User image files remain unchanged.
pub(crate) fn circular(source: &Image) -> Image {
    let Some(pixels) = source.to_rgba8() else {
        return source.clone();
    };
    let width = pixels.width();
    let height = pixels.height();
    let crop = width.min(height);
    if crop == 0 {
        return Image::default();
    }
    let edge = crop.clamp(64, MAX_AVATAR_EDGE);
    let origin_x = (width - crop) as f32 / 2.0;
    let origin_y = (height - crop) as f32 / 2.0;
    let scale = crop as f32 / edge as f32;
    let radius = edge as f32 / 2.0;
    let input = pixels.as_slice();
    let mut output = SharedPixelBuffer::<Rgba8Pixel>::new(edge, edge);
    for (index, pixel) in output.make_mut_slice().iter_mut().enumerate() {
        let x = (index % edge as usize) as f32;
        let y = (index / edge as usize) as f32;
        let sx = (origin_x + (x + 0.5) * scale - 0.5).clamp(0.0, (width - 1) as f32);
        let sy = (origin_y + (y + 0.5) * scale - 0.5).clamp(0.0, (height - 1) as f32);
        let x0 = sx.floor() as u32;
        let y0 = sy.floor() as u32;
        let x1 = (x0 + 1).min(width - 1);
        let y1 = (y0 + 1).min(height - 1);
        let fx = sx - x0 as f32;
        let fy = sy - y0 as f32;
        let a = input[y0 as usize * width as usize + x0 as usize];
        let b = input[y0 as usize * width as usize + x1 as usize];
        let c = input[y1 as usize * width as usize + x0 as usize];
        let d = input[y1 as usize * width as usize + x1 as usize];
        let blend = |a: u8, b: u8, c: u8, d: u8| {
            ((a as f32 * (1.0 - fx) + b as f32 * fx) * (1.0 - fy)
                + (c as f32 * (1.0 - fx) + d as f32 * fx) * fy)
                .round() as u8
        };
        pixel.r = blend(a.r, b.r, c.r, d.r);
        pixel.g = blend(a.g, b.g, c.g, d.g);
        pixel.b = blend(a.b, b.b, c.b, d.b);
        let dx = x + 0.5 - radius;
        let dy = y + 0.5 - radius;
        let coverage = (radius - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
        pixel.a = (blend(a.a, b.a, c.a, d.a) as f32 * coverage).round() as u8;
    }
    Image::from_rgba8(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_corners_without_changing_opaque_center_color() {
        let source = Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
            &[40, 90, 150, 255].repeat(64),
            8,
            8,
        ));
        let pixels = circular(&source).to_rgba8().unwrap();
        assert_eq!(pixels.as_slice()[0].a, 0);
        assert_eq!(pixels.as_slice()[pixels.width() as usize - 1].a, 0);
        let center = pixels.as_slice()
            [pixels.height() as usize / 2 * pixels.width() as usize + pixels.width() as usize / 2];
        assert_eq!((center.r, center.g, center.b, center.a), (40, 90, 150, 255));
    }

    #[test]
    fn center_crops_and_bounds_large_avatars() {
        let mut source = SharedPixelBuffer::<Rgba8Pixel>::new(600, 400);
        for (index, pixel) in source.make_mut_slice().iter_mut().enumerate() {
            let outer = !(100..500).contains(&(index % 600));
            pixel.r = if outer { 255 } else { 12 };
            pixel.g = if outer { 0 } else { 34 };
            pixel.b = if outer { 0 } else { 56 };
            pixel.a = 255;
        }
        let output = circular(&Image::from_rgba8(source)).to_rgba8().unwrap();
        assert!(output.width() <= MAX_AVATAR_EDGE);
        assert_eq!(output.width(), output.height());
        let edge = output.width() as usize;
        let middle = output.as_slice()[edge / 2 * edge + edge / 8];
        assert_eq!((middle.r, middle.g, middle.b, middle.a), (12, 34, 56, 255));
    }

    #[test]
    fn single_pixel_sources_still_produce_a_circular_mask() {
        let source = Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
            &[90, 140, 30, 255],
            1,
            1,
        ));
        let output = circular(&source).to_rgba8().unwrap();
        let edge = output.width() as usize;
        assert_eq!(output.as_slice()[0].a, 0);
        assert_eq!(output.as_slice()[edge / 2 * edge + edge / 2].a, 255);
    }
}

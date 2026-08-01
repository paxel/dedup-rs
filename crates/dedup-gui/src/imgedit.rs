//! 90°-step rotation and mirroring for the shared viewer's image tools. The
//! operations are applied to the decoded pixels for display only — matching a
//! copy somebody flipped is a *viewing* act; nothing is written back to disk.

use image::DynamicImage;

/// A single 90°-step rotation or mirror applied to the shown image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Orient {
    RotateCw,
    FlipH,
}

/// Apply `ops` in order to `img`.
pub fn apply_ops(mut img: DynamicImage, ops: &[Orient]) -> DynamicImage {
    for op in ops {
        img = match op {
            Orient::RotateCw => img.rotate90(),
            Orient::FlipH => img.fliph(),
        };
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage};

    #[test]
    fn apply_ops_compose_to_identity() {
        let img =
            DynamicImage::ImageRgb8(RgbImage::from_fn(4, 6, |x, y| Rgb([x as u8, y as u8, 7])));
        // Two horizontal flips cancel; four CW rotations return to start.
        assert_eq!(
            apply_ops(img.clone(), &[Orient::FlipH, Orient::FlipH]).to_rgb8(),
            img.to_rgb8()
        );
        assert_eq!(
            apply_ops(
                img.clone(),
                &[
                    Orient::RotateCw,
                    Orient::RotateCw,
                    Orient::RotateCw,
                    Orient::RotateCw
                ]
            )
            .to_rgb8(),
            img.to_rgb8()
        );
    }
}

//! End-to-end: `update` computes MIME + image fingerprints, and `find_similar`
//! groups perceptually similar images that are not byte-identical.

use dedup_core::store::Store;
use dedup_core::update::{CancellationToken, NoProgress, update_repo};
use image::{DynamicImage, Rgb, RgbImage};
use std::path::Path;

/// An asymmetric "L" shape on black — a recognizable perceptual signature.
fn l_shape() -> RgbImage {
    let mut img = RgbImage::from_pixel(100, 100, Rgb([0, 0, 0]));
    for y in 10..90 {
        for x in 10..30 {
            img.put_pixel(x, y, Rgb([255, 255, 255]));
        }
    }
    for y in 70..90 {
        for x in 10..70 {
            img.put_pixel(x, y, Rgb([255, 255, 255]));
        }
    }
    img
}

fn write_png(dir: &Path, name: &str, img: &RgbImage) {
    DynamicImage::ImageRgb8(img.clone())
        .save(dir.join(name))
        .expect("write test png");
}

#[test]
fn update_computes_fingerprints_and_find_similar_groups_them() {
    let config = tempfile::tempdir().expect("config dir");
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let store = Store::open_at(config.path().to_path_buf()).expect("open store");

    // Two near-identical L-shapes (a small background stamp changes the content
    // hash but not the perceptual hash) plus one clearly different image.
    let base = l_shape();
    let mut near = base.clone();
    for y in 2..6 {
        for x in 92..96 {
            near.put_pixel(x, y, Rgb([40, 40, 40]));
        }
    }
    let solid = RgbImage::from_pixel(100, 100, Rgb([128, 128, 128]));

    write_png(repo_dir.path(), "a.png", &base);
    write_png(repo_dir.path(), "b.png", &near);
    write_png(repo_dir.path(), "c.png", &solid);

    store
        .create_repo("pics", &repo_dir.path().to_string_lossy())
        .expect("create repo");
    let stats =
        update_repo(&store, "pics", 1, &NoProgress, &CancellationToken::new()).expect("update");
    assert_eq!(stats.added, 3);

    // The two L-shapes are not exact duplicates (different content hash).
    let exact = dedup_core::dupes::find_exact_duplicates(&store, &["pics".to_string()])
        .expect("exact dupes");
    assert!(
        exact.is_empty(),
        "similar-but-distinct files are not exact dupes"
    );

    // Fingerprints and MIME were persisted during update.
    let a = store
        .get_file_entry("pics", "a.png")
        .expect("get a.png")
        .expect("a.png entry");
    assert_eq!(a.mime.as_deref(), Some("image/png"));
    assert!(a.img_fingerprint.is_some(), "image fingerprint set");
    assert_eq!(a.img_size, Some((100, 100)));

    // Similarity search groups a.png and b.png but excludes the solid image.
    let groups = dedup_core::similar::find_similar(&store, &["pics".to_string()], 80.0)
        .expect("find similar");
    assert_eq!(groups.len(), 1, "exactly one similar group");
    let paths: Vec<&str> = groups[0].iter().map(|f| f.rel_path.as_str()).collect();
    assert_eq!(groups[0].len(), 2, "group holds both L-shapes");
    assert!(paths.contains(&"a.png") && paths.contains(&"b.png"));
    assert!(!paths.contains(&"c.png"), "the solid image is not similar");
}

//! Bounded image loading and decoding on the git worker, shared by both UIs.
use std::collections::VecDeque;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use super::repo::{DiffTarget, Repo};
use iced_core::image::Handle;

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const PREVIEW_EDGE: u32 = 1600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub handle: Handle,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSide {
    Missing,
    Ready(Preview),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageComparison {
    pub before: ImageSide,
    pub after: ImageSide,
    pub svg: bool,
}

/// Four decoded sides at most, each bounded to 1600 x 1600 RGBA pixels.
#[derive(Default)]
pub struct ImageCache(VecDeque<(git2::Oid, bool, ImageSide)>);

pub fn is_svg(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
}

fn supported(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg"
            )
        })
}

impl ImageCache {
    fn decode(&mut self, bytes: &[u8], svg: bool) -> ImageSide {
        if bytes.len() as u64 > MAX_BYTES {
            return ImageSide::Error("Image exceeds the 16 MB preview limit".into());
        }
        let key = git2::Oid::hash_object(git2::ObjectType::Blob, bytes).expect("hash image bytes");
        if let Some(i) = self.0.iter().position(|(k, s, _)| *k == key && *s == svg) {
            let entry = self.0.remove(i).expect("cache entry");
            let result = entry.2.clone();
            self.0.push_front(entry);
            return result;
        }
        let result = decode(bytes, svg)
            .map(ImageSide::Ready)
            .unwrap_or_else(|e| ImageSide::Error(format!("Cannot preview image: {e}")));
        self.0.push_front((key, svg, result.clone()));
        self.0.truncate(4);
        result
    }
}

fn decode(bytes: &[u8], svg: bool) -> Result<Preview, String> {
    let (width, height, rgba, rendered_width, rendered_height) = if svg {
        static FONTS: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
        let mut options = resvg::usvg::Options {
            fontdb: FONTS
                .get_or_init(|| {
                    let mut db = resvg::usvg::fontdb::Database::new();
                    db.load_system_fonts();
                    Arc::new(db)
                })
                .clone(),
            ..Default::default()
        };
        // A Git blob must not read unrelated files from the user's machine.
        options.image_href_resolver.resolve_string = Box::new(|_, _| None);
        let tree = resvg::usvg::Tree::from_data(bytes, &options).map_err(|e| e.to_string())?;
        let size = tree.size();
        let scale = (PREVIEW_EDGE as f32 / size.width().max(size.height())).min(1.0);
        let w = (size.width() * scale).ceil().max(1.0) as u32;
        let h = (size.height() * scale).ceil().max(1.0) as u32;
        let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or("Invalid SVG dimensions")?;
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        // tiny-skia is premultiplied; iced image handles require straight alpha.
        let rgba = pixmap
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect();
        (
            size.width().ceil() as u32,
            size.height().ceil() as u32,
            rgba,
            w,
            h,
        )
    } else {
        let mut reader = image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(16384);
        limits.max_image_height = Some(16384);
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        let decoder = reader.into_decoder().map_err(|e| e.to_string())?;
        let (w, h) = image::ImageDecoder::dimensions(&decoder);
        if u64::from(w) * u64::from(h) > MAX_PIXELS {
            return Err("Image exceeds the 16 megapixel preview limit".into());
        }
        let decoded = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
        let preview = if w.max(h) > PREVIEW_EDGE {
            decoded.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE)
        } else {
            decoded
        };
        let rgba = preview.into_rgba8();
        let (rw, rh) = rgba.dimensions();
        (w, h, rgba.into_raw(), rw, rh)
    };
    Ok(Preview {
        handle: Handle::from_rgba(rendered_width, rendered_height, rgba),
        width,
        height,
        bytes: bytes.len(),
    })
}

impl Repo {
    pub fn image_comparison(&self, target: &DiffTarget) -> Option<ImageComparison> {
        if !supported(target.path()) {
            return None;
        }
        let svg = is_svg(target.path());
        Some(
            self.load_image_comparison(target)
                .unwrap_or_else(|e| ImageComparison {
                    before: ImageSide::Error(e.to_string()),
                    after: ImageSide::Error(e.to_string()),
                    svg,
                }),
        )
    }

    fn load_image_comparison(&self, target: &DiffTarget) -> Result<ImageComparison, git2::Error> {
        // Discover renames before filtering to the selected file, so the old
        // side is read from the original path even when its extension changed.
        let mut opts = git2::DiffOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true);
        let index = self.index()?;
        let mut diff = match target {
            DiffTarget::WorkdirUnstaged(_) => self
                .repo
                .diff_index_to_workdir(Some(&index), Some(&mut opts))?,
            DiffTarget::Staged(_) => {
                let tree = self.repo.head().ok().and_then(|h| h.peel_to_tree().ok());
                self.repo
                    .diff_tree_to_index(tree.as_ref(), Some(&index), Some(&mut opts))?
            }
            DiffTarget::Commit(oid, _) => {
                let commit = self.repo.find_commit(*oid)?;
                let tree = commit.tree()?;
                let parent = commit.parent(0).ok().and_then(|p| p.tree().ok());
                self.repo
                    .diff_tree_to_tree(parent.as_ref(), Some(&tree), Some(&mut opts))?
            }
        };
        let mut find = git2::DiffFindOptions::new();
        find.renames(true).for_untracked(true);
        diff.find_similar(Some(&mut find))?;
        let path = Path::new(target.path());
        let delta = diff
            .deltas()
            .find(|d| d.new_file().path() == Some(path) || d.old_file().path() == Some(path));
        let Some(delta) = delta else {
            return Ok(ImageComparison {
                before: ImageSide::Missing,
                after: ImageSide::Missing,
                svg: is_svg(target.path()),
            });
        };
        let old = delta.old_file();
        let new = delta.new_file();
        let before = self.blob_image(old.id(), old.mode(), old.path().unwrap_or(path));
        let after = if delta.status() == git2::Delta::Deleted {
            ImageSide::Missing
        } else if matches!(target, DiffTarget::WorkdirUnstaged(_)) {
            self.workdir_image(new.path().unwrap_or(path))
        } else {
            self.blob_image(new.id(), new.mode(), new.path().unwrap_or(path))
        };
        Ok(ImageComparison {
            before,
            after,
            svg: is_svg(target.path()),
        })
    }

    fn blob_image(&self, oid: git2::Oid, mode: git2::FileMode, path: &Path) -> ImageSide {
        if oid.is_zero() {
            return ImageSide::Missing;
        }
        if !matches!(mode, git2::FileMode::Blob | git2::FileMode::BlobExecutable) {
            return ImageSide::Error("Not a regular image file".into());
        }
        match self.repo.find_blob(oid) {
            Ok(blob) => self
                .image_cache
                .borrow_mut()
                .decode(blob.content(), is_svg(&path.to_string_lossy())),
            Err(e) => ImageSide::Error(e.to_string()),
        }
    }

    fn workdir_image(&self, path: &Path) -> ImageSide {
        let read = || -> Result<Vec<u8>, std::io::Error> {
            let file = self.workdir.join(path);
            if !std::fs::symlink_metadata(&file)?.file_type().is_file() {
                return Err(std::io::Error::other("Not a regular image file"));
            }
            let mut bytes = Vec::new();
            std::fs::File::open(file)?
                .take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)?;
            Ok(bytes)
        };
        match read() {
            Ok(bytes) => self
                .image_cache
                .borrow_mut()
                .decode(&bytes, is_svg(&path.to_string_lossy())),
            Err(e) => ImageSide::Error(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::repo::{testutil::TempRepo, DiffOpts};

    fn svg(width: u32) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="20"><rect width="100%" height="100%" fill="red" opacity="0.5"/></svg>"#
        )
    }

    fn width(side: &ImageSide) -> u32 {
        match side {
            ImageSide::Ready(p) => p.width,
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn svg_alpha_cache_and_malformed_input() {
        let mut cache = ImageCache::default();
        let first = cache.decode(svg(20).as_bytes(), true);
        assert_eq!(first, cache.decode(svg(20).as_bytes(), true));
        let ImageSide::Ready(p) = first else {
            panic!("SVG failed")
        };
        let Handle::Rgba { pixels, .. } = p.handle else {
            panic!("not decoded")
        };
        assert_eq!(&pixels.as_ref()[0..4], &[255, 0, 0, 128]);
        assert!(matches!(
            cache.decode(b"<svg>broken", true),
            ImageSide::Error(_)
        ));
        assert!(matches!(
            cache.decode(b"not an image", false),
            ImageSide::Error(_)
        ));
        for n in 21..27 {
            cache.decode(svg(n).as_bytes(), true);
        }
        assert_eq!(cache.0.len(), 4);
    }

    #[test]
    fn supported_raster_formats_decode() {
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::Jpeg,
            image::ImageFormat::Gif,
            image::ImageFormat::WebP,
            image::ImageFormat::Bmp,
            image::ImageFormat::Ico,
        ] {
            let mut encoded = Cursor::new(Vec::new());
            let raster = if format == image::ImageFormat::Jpeg {
                image::DynamicImage::new_rgb8(8, 4)
            } else {
                image::DynamicImage::new_rgba8(8, 4)
            };
            raster.write_to(&mut encoded, format).unwrap();
            let preview = decode(encoded.get_ref(), false).unwrap();
            assert_eq!((preview.width, preview.height), (8, 4), "{format:?}");
        }
        assert!(supported("PICTURE.SVG"));
        assert!(!supported("notes.txt"));
    }

    #[test]
    fn bounded_decode_and_svg_external_resources() {
        let preview = decode(svg(10000).as_bytes(), true).unwrap();
        let Handle::Rgba { width, height, .. } = preview.handle else {
            panic!()
        };
        assert_eq!(width, PREVIEW_EDGE);
        assert!(height <= PREVIEW_EDGE);
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::new_luma8(5000, 4000)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        assert!(decode(encoded.get_ref(), false)
            .unwrap_err()
            .contains("megapixel"));
        assert!(matches!(
            ImageCache::default().decode(&vec![0; MAX_BYTES as usize + 1], false),
            ImageSide::Error(_)
        ));
        let t = TempRepo::new();
        std::fs::write(t.dir.join("external.png"), crate::ui::logo::BYTES).unwrap();
        let external = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><image href="{}" width="10" height="10"/></svg>"#,
            t.dir.join("external.png").display()
        );
        let preview = decode(external.as_bytes(), true).unwrap();
        let Handle::Rgba { pixels, .. } = preview.handle else {
            panic!()
        };
        assert!(pixels.as_ref().iter().all(|b| *b == 0));
    }

    #[test]
    fn worktree_index_and_commit_are_distinct_and_refresh() {
        let t = TempRepo::new();
        t.write("art.svg", &svg(10));
        t.add("art.svg");
        let initial = t.commit("initial");
        let r = Repo::open(&t.dir).unwrap();
        t.write("art.svg", &svg(20));
        t.add("art.svg");
        t.write("art.svg", &svg(30));
        let unstaged = r
            .diff(
                &DiffTarget::WorkdirUnstaged("art.svg".into()),
                DiffOpts::default(),
            )
            .unwrap();
        assert!(!unstaged.hunks.is_empty(), "SVG source diff retained");
        let images = unstaged.images.unwrap();
        assert_eq!((width(&images.before), width(&images.after)), (20, 30));
        let staged = r
            .image_comparison(&DiffTarget::Staged("art.svg".into()))
            .unwrap();
        assert_eq!((width(&staged.before), width(&staged.after)), (10, 20));
        let root = r
            .image_comparison(&DiffTarget::Commit(initial, "art.svg".into()))
            .unwrap();
        assert_eq!(root.before, ImageSide::Missing);
        assert_eq!(width(&root.after), 10);
        let next = t.commit("staged version");
        let commit = r
            .image_comparison(&DiffTarget::Commit(next, "art.svg".into()))
            .unwrap();
        assert_eq!((width(&commit.before), width(&commit.after)), (10, 20));
        t.write("art.svg", &svg(40));
        let refreshed = r
            .image_comparison(&DiffTarget::WorkdirUnstaged("art.svg".into()))
            .unwrap();
        assert_eq!(width(&refreshed.after), 40);
    }

    #[test]
    fn untracked_added_deleted_and_renamed() {
        let t = TempRepo::new();
        let r = Repo::open(&t.dir).unwrap();
        t.write("art.svg", &svg(10));
        let images = r
            .image_comparison(&DiffTarget::WorkdirUnstaged("art.svg".into()))
            .unwrap();
        assert_eq!(images.before, ImageSide::Missing);
        assert_eq!(width(&images.after), 10);
        t.add("art.svg");
        let images = r
            .image_comparison(&DiffTarget::Staged("art.svg".into()))
            .unwrap();
        assert_eq!(images.before, ImageSide::Missing);
        assert_eq!(width(&images.after), 10);
        t.commit("initial");
        std::fs::rename(t.dir.join("art.svg"), t.dir.join("new.svg")).unwrap();
        let images = r
            .image_comparison(&DiffTarget::WorkdirUnstaged("new.svg".into()))
            .unwrap();
        assert_eq!((width(&images.before), width(&images.after)), (10, 10));
        r.stage(&["art.svg".into(), "new.svg".into()]).unwrap();
        let images = r
            .image_comparison(&DiffTarget::Staged("new.svg".into()))
            .unwrap();
        assert_eq!((width(&images.before), width(&images.after)), (10, 10));
        let renamed = t.commit("rename");
        let images = r
            .image_comparison(&DiffTarget::Commit(renamed, "new.svg".into()))
            .unwrap();
        assert_eq!((width(&images.before), width(&images.after)), (10, 10));
        std::fs::remove_file(t.dir.join("new.svg")).unwrap();
        let images = r
            .image_comparison(&DiffTarget::WorkdirUnstaged("new.svg".into()))
            .unwrap();
        assert_eq!(width(&images.before), 10);
        assert_eq!(images.after, ImageSide::Missing);
        r.stage(&["new.svg".into()]).unwrap();
        let images = r
            .image_comparison(&DiffTarget::Staged("new.svg".into()))
            .unwrap();
        assert_eq!(width(&images.before), 10);
        assert_eq!(images.after, ImageSide::Missing);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let t = TempRepo::new();
        std::os::unix::fs::symlink("/etc/passwd", t.dir.join("art.svg")).unwrap();
        let r = Repo::open(&t.dir).unwrap();
        let images = r
            .image_comparison(&DiffTarget::WorkdirUnstaged("art.svg".into()))
            .unwrap();
        assert!(matches!(images.after, ImageSide::Error(_)));
    }
}

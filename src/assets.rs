//! Workspace asset store for embedded images（图片元素的字节存放处）。
//!
//! 插入图片时把字节复制进 `<workspace>/.boundless/assets/`，场景元素只存
//! 文件名 —— 原文件被移动/删除后画板依然完好。解码结果按文件名缓存
//! （首次绘制同步解码，之后是缓存命中）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::RenderImage;

pub struct AssetStore {
    dir: PathBuf,
    cache: RefCell<HashMap<String, Arc<RenderImage>>>,
}

impl AssetStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            cache: RefCell::new(HashMap::new()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Store raw bytes under a fresh unique name（`img-<uuid>.<ext>`），
    /// 目录不存在时自动创建。返回资产文件名。
    pub fn store(&self, bytes: &[u8], ext: &str) -> std::io::Result<String> {
        std::fs::create_dir_all(&self.dir)?;
        let name = format!(
            "img-{}.{}",
            uuid::Uuid::new_v4(),
            ext.trim_start_matches('.')
        );
        std::fs::write(self.dir.join(&name), bytes)?;
        Ok(name)
    }

    /// Decode + cache an asset by file name。缺失/损坏的资产返回 None
    /// （不缓存负结果 —— 文件可能稍后被补上）。
    pub fn load(&self, name: &str) -> Option<Arc<RenderImage>> {
        if let Some(img) = self.cache.borrow().get(name) {
            return Some(img.clone());
        }
        let bytes = std::fs::read(self.dir.join(name)).ok()?;
        // RenderImage frames are RGBA; image::open covers png/jpeg/gif/webp.
        let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
        let render = Arc::new(RenderImage::new(vec![image::Frame::new(img)]));
        self.cache.borrow_mut().insert(name.to_string(), render.clone());
        Some(render)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> AssetStore {
        let dir = std::env::temp_dir().join(format!("boundless-assets-{}", uuid::Uuid::new_v4()));
        AssetStore::new(dir)
    }

    fn tiny_png() -> Vec<u8> {
        // 2x1 红蓝 PNG，现生成的最小合法图片。
        let mut buf = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(std::io::Cursor::new(&mut buf));
        image::ImageEncoder::write_image(
            encoder,
            &[255, 0, 0, 255, 0, 0, 255, 255],
            2,
            1,
            image::ExtendedColorType::Rgba8,
        )
        .expect("encode tiny png");
        buf
    }

    #[test]
    fn store_writes_file_and_returns_unique_names() {
        let store = temp_store();
        let a = store.store(&tiny_png(), "png").expect("store a");
        let b = store.store(&tiny_png(), "png").expect("store b");
        assert_ne!(a, b, "each store call must mint a fresh name");
        assert!(a.starts_with("img-") && a.ends_with(".png"));
        assert!(store.dir().join(&a).is_file(), "bytes must hit the disk");
    }

    #[test]
    fn store_creates_missing_directory() {
        let dir = std::env::temp_dir().join(format!("boundless-assets-{}", uuid::Uuid::new_v4()));
        assert!(!dir.exists());
        let store = AssetStore::new(dir.clone());
        store.store(&tiny_png(), "png").expect("store");
        assert!(dir.is_dir(), "store must mkdir the asset dir");
    }

    #[test]
    fn load_decodes_and_caches() {
        let store = temp_store();
        let name = store.store(&tiny_png(), "png").expect("store");
        let first = store.load(&name).expect("decode");
        let size = first.size(0);
        assert_eq!(u32::from(size.width), 2, "decoded width");
        assert_eq!(u32::from(size.height), 1, "decoded height");
        let second = store.load(&name).expect("cache hit");
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "second load must be the cached Arc"
        );
    }

    #[test]
    fn load_missing_or_corrupt_is_none() {
        let store = temp_store();
        assert!(store.load("img-nonexistent.png").is_none());
        let name = store.store(b"not an image", "png").expect("store junk");
        assert!(store.load(&name).is_none(), "corrupt bytes must not decode");
    }

    #[test]
    fn extension_is_normalized() {
        let store = temp_store();
        let name = store.store(&tiny_png(), ".PNG").expect("store");
        assert!(name.ends_with(".PNG"), "dot trimmed only, case kept: {name}");
    }
}

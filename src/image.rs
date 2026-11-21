//! Image/map consistency and durable checkpoint ordering.
use crate::rescue_map::RescueMap;
use std::os::windows::fs::OpenOptionsExt;
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::Path,
};
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

pub(crate) struct ImageFile(Option<File>);
impl std::ops::Deref for ImageFile {
    type Target = File;
    fn deref(&self) -> &File {
        self.0.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for ImageFile {
    fn deref_mut(&mut self) -> &mut File {
        self.0.as_mut().unwrap()
    }
}
impl Drop for ImageFile {
    fn drop(&mut self) {
        let _operation = crate::watchdog::operation("close image file");
        drop(self.0.take());
    }
}

pub(crate) fn exists(path: &str) -> io::Result<bool> {
    let _operation = crate::watchdog::operation("inspect resume files");
    Path::new(path).try_exists()
}

pub(crate) fn ensure_new_destination(path: &str) -> io::Result<()> {
    let _operation = crate::watchdog::operation("check existing image");
    if Path::new(path).exists() && fs::metadata(path)?.len() != 0 {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, tr!(
            "The output image exists without a matching map. Choose a new output path or restore its matching map.",
            "対応するマップがない既存イメージを上書きしません。別の出力先を選ぶか、対応するマップを戻してください。")));
    }
    Ok(())
}

pub(crate) fn open(path: &str, total_bytes: u64, recovered: bool) -> io::Result<ImageFile> {
    let _operation = crate::watchdog::operation("open image");
    let invalid = || {
        io::Error::new(io::ErrorKind::InvalidData, tr!(
            "The map contains recovered sectors, but its matching full-size image is missing or has a different size. No image data was modified.",
            "マップに救出済みセクタがありますが、対応する全容量のイメージが存在しないか、サイズが一致しません。イメージは変更していません。"))
    };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(!recovered)
        .truncate(false)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)
        .map_err(|error| {
            if recovered && error.kind() == io::ErrorKind::NotFound {
                invalid()
            } else {
                error
            }
        })?;
    if recovered && file.metadata()?.len() != total_bytes {
        return Err(invalid());
    }
    if file.metadata()?.len() != total_bytes {
        file.set_len(total_bytes)?;
    }
    Ok(ImageFile(Some(file)))
}

pub(crate) fn checkpoint(file: &File, map: &RescueMap, path: &str) -> io::Result<()> {
    let _operation = crate::watchdog::operation("durable checkpoint");
    // Never persist '+' in the map before its image bytes have been flushed.
    file.sync_data()?;
    map.save(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rescue_map::SectorStatus;
    use std::io::{Read, Seek, SeekFrom, Write};
    #[test]
    fn missing_or_wrong_size_image_never_silently_recreates_recovered_data() -> io::Result<()> {
        let path = std::env::temp_dir().join(format!(
            "sd2img-image-validation-{}.img",
            std::process::id()
        ));
        let name = path.to_str().unwrap();
        assert!(open(name, 1024, true).is_err());
        assert!(!path.exists());
        fs::write(&path, b"preserve me")?;
        assert!(open(name, 1024, true).is_err());
        assert_eq!(fs::read(&path)?, b"preserve me");
        assert!(ensure_new_destination(name).is_err());
        fs::remove_file(path)
    }
    #[test]
    fn checkpoint_and_reopen_preserve_data_and_skip_only_recorded_sectors() -> io::Result<()> {
        let path =
            std::env::temp_dir().join(format!("sd2img-checkpoint-{}.img", std::process::id()));
        let name = path.to_str().unwrap();
        let map_path = format!("{name}.map");
        let data = vec![0x5a; 512];
        let mut map = RescueMap::new(0, data.clone(), 2);
        let mut file = open(name, 1024, false)?;
        file.write_all(&data)?;
        map.update_range(0, 1, SectorStatus::Rescued);
        checkpoint(&file, &map, &map_path)?;
        drop(file);
        let restored = RescueMap::load(&map_path)?;
        restored.validate(2)?;
        let mut file = open(name, 1024, restored.stats().0 > 0)?;
        let mut actual = vec![0; 512];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut actual)?;
        assert_eq!(actual, data);
        assert_eq!(restored.stats(), (1, 0, 1));
        drop(file);
        fs::remove_file(path)?;
        fs::remove_file(map_path)
    }
}

use std::fs;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

#[cfg(test)]
pub fn write_tar(src_root: &Path, out: &mut impl Write) -> Result<()> {
    write_tar_as(src_root, out, 0, 0)
}

pub fn write_tar_as(src_root: &Path, out: &mut impl Write, uid: u32, gid: u32) -> Result<()> {
    if src_root.is_dir() {
        write_dir_children(src_root, src_root, out, uid, gid)?;
    }
    out.write_all(&[0; 1024])
        .context("failed to finish tar stream")?;
    Ok(())
}

fn write_dir_children(
    root: &Path,
    dir: &Path,
    out: &mut impl Write,
    uid: u32,
    gid: u32,
) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to list {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .with_context(|| format!("failed to relativize {}", path.display()))?;
        write_entry(root, relative, &path, out, uid, gid)?;
    }
    Ok(())
}

fn write_entry(
    root: &Path,
    relative: &Path,
    path: &Path,
    out: &mut impl Write,
    uid: u32,
    gid: u32,
) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    let file_type = metadata.file_type();
    let tar_path = tar_path(relative)?;

    if file_type.is_dir() {
        let name = if tar_path.ends_with('/') {
            tar_path
        } else {
            format!("{tar_path}/")
        };
        write_header(out, &name, 0, 0o755, uid, gid, b'5', "")?;
        write_dir_children(root, path, out, uid, gid)?;
        return Ok(());
    }

    if file_type.is_file() {
        write_header(out, &tar_path, metadata.len(), 0o644, uid, gid, b'0', "")?;
        let mut input =
            fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let copied = std::io::copy(&mut input, out)
            .with_context(|| format!("failed to archive {}", path.display()))?;
        pad_512(out, copied)?;
        return Ok(());
    }

    #[cfg(unix)]
    if file_type.is_symlink() {
        let target = fs::read_link(path)
            .with_context(|| format!("failed to read symlink {}", path.display()))?;
        let link_name = target
            .to_str()
            .with_context(|| format!("non-utf8 symlink target {}", path.display()))?;
        if link_name.as_bytes().len() > 100 {
            bail!("symlink target too long for ustar: {}", path.display());
        }
        write_header(out, &tar_path, 0, 0o777, uid, gid, b'2', link_name)?;
        return Ok(());
    }

    #[cfg(unix)]
    if file_type.is_fifo()
        || file_type.is_socket()
        || file_type.is_char_device()
        || file_type.is_block_device()
    {
        return Ok(());
    }

    Ok(())
}

fn tar_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for part in path.components() {
        match part {
            std::path::Component::Normal(p) => {
                let s = p
                    .to_str()
                    .with_context(|| format!("non-utf8 path in staged home: {}", path.display()))?;
                parts.push(s);
            }
            _ => bail!("unsupported archive path {}", path.display()),
        }
    }
    if parts.is_empty() {
        bail!("refusing to archive empty relative path");
    }
    Ok(parts.join("/"))
}

fn write_header(
    out: &mut impl Write,
    path: &str,
    size: u64,
    mode: u64,
    uid: u32,
    gid: u32,
    typeflag: u8,
    link_name: &str,
) -> Result<()> {
    let mut header = [0u8; 512];
    write_name(&mut header, path)?;
    write_octal(&mut header[100..108], mode)?;
    write_octal(&mut header[108..116], u64::from(uid))?;
    write_octal(&mut header[116..124], u64::from(gid))?;
    write_octal(&mut header[124..136], size)?;
    write_octal(&mut header[136..148], 0)?;
    for b in &mut header[148..156] {
        *b = b' ';
    }
    header[156] = typeflag;
    write_bytes(&mut header[157..257], link_name.as_bytes())?;
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u32 = header.iter().map(|b| u32::from(*b)).sum();
    write_checksum(&mut header[148..156], checksum)?;
    out.write_all(&header)
        .with_context(|| format!("failed to write tar header for {path}"))?;
    Ok(())
}

fn write_name(header: &mut [u8; 512], path: &str) -> Result<()> {
    let bytes = path.as_bytes();
    if bytes.len() <= 100 {
        write_bytes(&mut header[0..100], bytes)?;
        return Ok(());
    }

    for split in path.match_indices('/').map(|(idx, _)| idx).rev() {
        let prefix = &path[..split];
        let name = &path[split + 1..];
        if prefix.as_bytes().len() <= 155 && name.as_bytes().len() <= 100 {
            write_bytes(&mut header[0..100], name.as_bytes())?;
            write_bytes(&mut header[345..500], prefix.as_bytes())?;
            return Ok(());
        }
    }
    bail!("path too long for ustar: {path}");
}

fn write_octal(field: &mut [u8], value: u64) -> Result<()> {
    let width = field.len();
    let s = format!("{value:0width$o}", width = width - 1);
    if s.len() >= width {
        bail!("tar numeric field overflow");
    }
    field.fill(0);
    field[..s.len()].copy_from_slice(s.as_bytes());
    Ok(())
}

fn write_checksum(field: &mut [u8], value: u32) -> Result<()> {
    let s = format!("{value:06o}\0 ");
    field.copy_from_slice(s.as_bytes());
    Ok(())
}

fn write_bytes(field: &mut [u8], bytes: &[u8]) -> Result<()> {
    if bytes.len() > field.len() {
        bail!("tar field overflow");
    }
    field[..bytes.len()].copy_from_slice(bytes);
    Ok(())
}

fn pad_512(out: &mut impl Write, len: u64) -> Result<()> {
    let rem = len % 512;
    if rem == 0 {
        return Ok(());
    }
    let pad = 512 - rem;
    let mut zeroes = std::io::repeat(0).take(pad);
    std::io::copy(&mut zeroes, out).context("failed to write tar padding")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_ustar_archive_with_files_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("home");
        fs::create_dir_all(root.join(".claude/skills/demo")).unwrap();
        fs::write(root.join(".claude.json"), "{}").unwrap();
        fs::write(root.join(".claude/skills/demo/SKILL.md"), "body").unwrap();
        let mut archive = Vec::new();

        write_tar(&root, &mut archive).unwrap();
        let body = archive;

        assert!(
            body.windows(".claude.json".len())
                .any(|w| w == b".claude.json")
        );
        assert!(
            body.windows(".claude/skills/demo/SKILL.md".len())
                .any(|w| w == b".claude/skills/demo/SKILL.md")
        );
        assert_eq!(body.len() % 512, 0);
    }

    #[test]
    fn writes_requested_uid_and_gid() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("home");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".claude.json"), "{}").unwrap();
        let mut archive = Vec::new();

        write_tar_as(&root, &mut archive, 501, 20).unwrap();

        let header = &archive[..512];
        assert_eq!(std::str::from_utf8(&header[108..115]).unwrap(), "0000765");
        assert_eq!(std::str::from_utf8(&header[116..123]).unwrap(), "0000024");
    }
}

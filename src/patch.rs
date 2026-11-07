//! Validated, length-preserving patch for the Electron ASAR integrity scheme.
use regex::bytes::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::OnceLock;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PatchError {
    #[error("文件读写失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("ASAR JSON 无效: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
}

type Result<T> = std::result::Result<T, PatchError>;

const PATTERN: &str = r#"(?P<c0>[A-Za-z_$][A-Za-z0-9_$]*)\[0\]!==(?P<x0>[A-Za-z_$][A-Za-z0-9_$]*)\?\((?P<t0>[A-Za-z_$][A-Za-z0-9_$]*)=(?P<x1>[A-Za-z_$][A-Za-z0-9_$]*)\?\.get\("enable_i18n",!1\),(?P<c1>[A-Za-z_$][A-Za-z0-9_$]*)\[0\]=(?P<x2>[A-Za-z_$][A-Za-z0-9_$]*),(?P<c2>[A-Za-z_$][A-Za-z0-9_$]*)\[1\]=(?P<t1>[A-Za-z_$][A-Za-z0-9_$]*)\):(?P<t2>[A-Za-z_$][A-Za-z0-9_$]*)=(?P<c3>[A-Za-z_$][A-Za-z0-9_$]*)\[1\]"#;

fn matcher() -> &'static Regex {
    static PAT: OnceLock<Regex> = OnceLock::new();
    PAT.get_or_init(|| Regex::new(PATTERN).expect("constant regex"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Matched {
    start: usize,
    end: usize,
    x: Vec<u8>,
    t: Vec<u8>,
}

fn matches(bytes: &[u8]) -> Vec<Matched> {
    let mut result = Vec::new();
    let mut at = 0;
    while let Some(c) = matcher().captures_at(bytes, at) {
        let full = c.get(0).unwrap();
        let equal = |a, b| c.name(a).unwrap().as_bytes() == c.name(b).unwrap().as_bytes();
        if ["c1", "c2", "c3"].into_iter().all(|n| equal("c0", n))
            && ["x1", "x2"].into_iter().all(|n| equal("x0", n))
            && ["t1", "t2"].into_iter().all(|n| equal("t0", n))
        {
            result.push(Matched {
                start: full.start(),
                end: full.end(),
                x: c["x0"].to_vec(),
                t: c["t0"].to_vec(),
            });
            at = full.end();
        } else {
            at = full.start() + 1;
        }
    }
    result
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(out, "{b:02x}").unwrap();
    }
    out
}
pub fn hash(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

#[derive(Debug)]
pub struct PatchPlan {
    pub resource: String,
    pub old_header_hash: String,
    pub new_header_hash: String,
    old_header: Vec<u8>,
    new_header: Vec<u8>,
    content_offset: u64,
    old_content_hash: String,
    new_content: Vec<u8>,
}

fn invalid(message: impl Into<String>) -> PatchError {
    PatchError::Invalid(message.into())
}

fn entries<'a>(node: &'a Value, prefix: &str, output: &mut Vec<(String, &'a Value)>) -> Result<()> {
    let files = node
        .get("files")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("ASAR 文件树无效"))?;
    for (name, item) in files {
        let path = format!("{prefix}/{name}");
        if item.get("files").is_some() {
            entries(item, &path, output)?;
        } else if path.starts_with("/webview/")
            && path.ends_with(".js")
            && item.get("offset").is_some()
        {
            output.push((path, item));
        }
    }
    Ok(())
}

fn replace_unique(buf: &mut [u8], before: &[u8], after: &[u8]) -> Result<()> {
    if before == after {
        return Ok(());
    }
    if before.is_empty() || before.len() != after.len() {
        return Err(invalid("替换长度无效"));
    }
    let found: Vec<_> = buf
        .windows(before.len())
        .enumerate()
        .filter_map(|(i, w)| (w == before).then_some(i))
        .take(2)
        .collect();
    if found.len() != 1 {
        return Err(invalid("校验值不存在或不唯一，已停止"));
    }
    buf[found[0]..found[0] + before.len()].copy_from_slice(after);
    Ok(())
}

/// Reads and validates the original archive and EXE without modifying them.
pub fn inspect(archive: &Path, exe: &Path) -> Result<PatchPlan> {
    let mut f = File::open(archive)?;
    let file_len = f.metadata()?.len();
    let mut prefix = [0_u8; 16];
    f.read_exact(&mut prefix)?;
    let n = |i| u32::from_le_bytes(prefix[i..i + 4].try_into().unwrap());
    let (magic, size, len) = (n(0), n(4) as u64, n(12) as usize);
    if magic != 4 || len > 100_000_000 || size < len as u64 + 8 || 16 + len as u64 > file_len {
        return Err(invalid("不支持的 ASAR 头部"));
    }
    let base = 8_u64
        .checked_add(size)
        .ok_or_else(|| invalid("ASAR 偏移溢出"))?;
    let mut header = vec![0; len];
    f.read_exact(&mut header)?;
    let tree: Value = serde_json::from_slice(&header)?;
    let mut paths = Vec::new();
    entries(&tree, "", &mut paths)?;
    let mut found = Vec::new();
    for (name, item) in paths {
        let offset = item["offset"]
            .as_str()
            .ok_or_else(|| invalid("ASAR 偏移无效"))?
            .parse::<u64>()
            .map_err(|_| invalid("ASAR 偏移无效"))?;
        let length = item["size"]
            .as_u64()
            .ok_or_else(|| invalid("ASAR 文件大小无效"))?;
        let abs = base
            .checked_add(offset)
            .ok_or_else(|| invalid("ASAR 偏移溢出"))?;
        if abs
            .checked_add(length)
            .filter(|end| *end <= file_len)
            .is_none()
        {
            return Err(invalid("ASAR 文件超出边界"));
        }
        // Reject unreasonable entries rather than allocating unbounded memory.
        if length > 256 * 1024 * 1024 {
            return Err(invalid("ASAR JavaScript 文件过大"));
        }
        let mut content = vec![0; length as usize];
        f.seek(SeekFrom::Start(abs))?;
        f.read_exact(&mut content)?;
        if !content.windows(11).any(|v| v == b"enable_i18n") {
            continue;
        }
        for m in matches(&content) {
            found.push((name.clone(), item, abs, content.clone(), m));
        }
    }
    if found.len() != 1 {
        return Err(invalid(format!(
            "找到 {} 处语言逻辑；预期唯一匹配，未作修改",
            found.len()
        )));
    }
    let (resource, item, abs, content, m) = found.pop().unwrap();
    let integrity = &item["integrity"];
    if integrity["algorithm"].as_str() != Some("SHA256")
        || integrity["hash"].as_str() != Some(&hash(&content))
    {
        return Err(invalid("资源完整性校验失败"));
    }
    let blocksize = integrity["blockSize"]
        .as_u64()
        .filter(|x| *x > 0 && *x <= usize::MAX as u64)
        .ok_or_else(|| invalid("ASAR 分块大小无效"))? as usize;
    let blocks = integrity["blocks"]
        .as_array()
        .ok_or_else(|| invalid("ASAR 分块哈希无效"))?;
    let oldblocks: Vec<_> = content.chunks(blocksize).map(hash).collect();
    if blocks.len() != oldblocks.len()
        || blocks
            .iter()
            .zip(&oldblocks)
            .any(|(a, b)| a.as_str() != Some(b))
    {
        return Err(invalid("资源分块校验失败"));
    }
    let mut replacement = m.t.clone();
    replacement.push(b'=');
    replacement.extend_from_slice(&m.x);
    replacement.extend_from_slice(b"?.get(\"enable_i18n\",!0)??!0");
    if replacement.len() > m.end - m.start {
        return Err(invalid("替换内容超出原有空间"));
    }
    replacement.resize(m.end - m.start, b' ');
    let mut updated = content.clone();
    updated[m.start..m.end].copy_from_slice(&replacement);
    let mut new_header = header.clone();
    replace_unique(
        &mut new_header,
        hash(&content).as_bytes(),
        hash(&updated).as_bytes(),
    )?;
    for (old, new) in oldblocks.iter().zip(updated.chunks(blocksize).map(hash)) {
        replace_unique(&mut new_header, old.as_bytes(), new.as_bytes())?;
    }
    let old_header_hash = hash(&header);
    let new_header_hash = hash(&new_header);
    let exe_data = std::fs::read(exe)?;
    if exe_data
        .windows(64)
        .filter(|w| *w == old_header_hash.as_bytes())
        .count()
        != 1
    {
        return Err(invalid("EXE 中 ASAR 校验值不存在或不唯一"));
    }
    Ok(PatchPlan {
        resource,
        old_header_hash,
        new_header_hash,
        old_header: header,
        new_header,
        content_offset: abs,
        old_content_hash: hash(&content),
        new_content: updated,
    })
}

/// Apply only to an isolated staging copy. An interrupted update is discarded with the staging directory.
pub fn apply(plan: &PatchPlan, archive: &Path, exe: &Path) -> Result<()> {
    let mut archive_file = File::options().read(true).write(true).open(archive)?;
    let mut header = vec![0; plan.old_header.len()];
    archive_file.seek(SeekFrom::Start(16))?;
    archive_file.read_exact(&mut header)?;
    let mut content = vec![0; plan.new_content.len()];
    archive_file.seek(SeekFrom::Start(plan.content_offset))?;
    archive_file.read_exact(&mut content)?;
    if header != plan.old_header || hash(&content) != plan.old_content_hash {
        return Err(invalid("暂存副本在修复前发生变化"));
    }
    let mut executable = std::fs::read(exe)?;
    replace_unique(
        &mut executable,
        plan.old_header_hash.as_bytes(),
        plan.new_header_hash.as_bytes(),
    )?;
    archive_file.seek(SeekFrom::Start(16))?;
    archive_file.write_all(&plan.new_header)?;
    archive_file.seek(SeekFrom::Start(plan.content_offset))?;
    archive_file.write_all(&plan.new_content)?;
    archive_file.sync_all()?;
    let mut exe_file = File::options().write(true).truncate(true).open(exe)?;
    exe_file.write_all(&executable)?;
    exe_file.sync_all()?;
    archive_file.seek(SeekFrom::Start(16))?;
    archive_file.read_exact(&mut header)?;
    archive_file.seek(SeekFrom::Start(plan.content_offset))?;
    archive_file.read_exact(&mut content)?;
    if header != plan.new_header || content != plan.new_content || std::fs::read(exe)? != executable
    {
        return Err(invalid("修复后验证失败"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const CODE: &str = "A[0]!==B?(C=B?.get(\"enable_i18n\",!1),A[0]=B,A[1]=C):C=A[1]";
    fn fixture(content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("app.asar");
        let exe = dir.path().join("ChatGPT.exe");
        let blocks: Vec<_> = content.chunks(64).map(hash).collect();
        let tree = serde_json::json!({"files":{"webview":{"files":{"asset.js":{"size":content.len(),"offset":"0","integrity":{"algorithm":"SHA256","hash":hash(content),"blockSize":64,"blocks":blocks}}}}}});
        let header = serde_json::to_vec(&tree).unwrap();
        let mut data = Vec::new();
        for value in [
            4u32,
            (header.len() + 8) as u32,
            (header.len() + 4) as u32,
            header.len() as u32,
        ] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(&header);
        data.extend_from_slice(content);
        std::fs::write(&archive, data).unwrap();
        std::fs::write(&exe, format!("exe-hash:{}-end", hash(&header))).unwrap();
        (dir, archive, exe)
    }
    fn content(code: &str) -> Vec<u8> {
        let mut data = String::new();
        for n in 0..30 {
            data.push_str(&format!("{n:04}-abcdefghijklmnop;"));
        }
        data.push_str(code);
        for n in 30..60 {
            data.push_str(&format!("{n:04}-qrstuvwxyzabcdef;"));
        }
        data.into_bytes()
    }
    #[test]
    fn fixture_validates_and_patches_in_place() {
        let (_dir, archive, exe) = fixture(&content(CODE));
        let plan = inspect(&archive, &exe).unwrap();
        assert_eq!(plan.resource, "/webview/asset.js");
        assert_ne!(plan.old_header_hash, plan.new_header_hash);
        apply(&plan, &archive, &exe).unwrap();
        assert!(
            std::fs::read(&exe)
                .unwrap()
                .windows(64)
                .any(|w| w == plan.new_header_hash.as_bytes())
        );
        assert!(inspect(&archive, &exe).is_err());
    }
    #[test]
    fn fixture_rejects_tamper_and_multiple_matches() {
        let (_dir, archive, exe) = fixture(&content(CODE));
        let mut bytes = std::fs::read(&archive).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&archive, bytes).unwrap();
        assert!(inspect(&archive, &exe).is_err());
        let (_dir, archive, exe) = fixture(&content(&format!("{CODE};{CODE}")));
        assert!(inspect(&archive, &exe).is_err());
    }
    #[test]
    fn matching_and_overlap() {
        let good = b"A[0]!==B?(C=B?.get(\"enable_i18n\",!1),A[0]=B,A[1]=C):C=A[1]";
        assert_eq!(matches(good).len(), 1);
        let overlap = [b"X".as_slice(), good].concat();
        assert_eq!(matches(&overlap)[0].start, 1);
        assert!(
            matches(b"A[0]!==B?(C=D?.get(\"enable_i18n\",!1),A[0]=B,A[1]=C):C=A[1]").is_empty()
        );
    }
    #[test]
    fn replacements_are_unique() {
        let mut b = b"abc abc".to_vec();
        assert!(replace_unique(&mut b, b"abc", b"def").is_err());
        let mut b = b"abc".to_vec();
        replace_unique(&mut b, b"abc", b"def").unwrap();
        assert_eq!(b, b"def");
    }
}

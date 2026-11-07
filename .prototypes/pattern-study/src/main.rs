use fancy_regex::Regex as FancyRegex;
use regex::bytes::Regex as ByteRegex;
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

// Same ASCII identifiers and literals as the Python bytes pattern. Every repeated
// identifier is captured separately and checked explicitly by checked_matches.
const CANDIDATE: &str = r#"(?P<c0>[A-Za-z_$][A-Za-z0-9_$]*)\[0\]!==(?P<x0>[A-Za-z_$][A-Za-z0-9_$]*)\?\((?P<t0>[A-Za-z_$][A-Za-z0-9_$]*)=(?P<x1>[A-Za-z_$][A-Za-z0-9_$]*)\?\.get\("enable_i18n",!1\),(?P<c1>[A-Za-z_$][A-Za-z0-9_$]*)\[0\]=(?P<x2>[A-Za-z_$][A-Za-z0-9_$]*),(?P<c2>[A-Za-z_$][A-Za-z0-9_$]*)\[1\]=(?P<t1>[A-Za-z_$][A-Za-z0-9_$]*)\):(?P<t2>[A-Za-z_$][A-Za-z0-9_$]*)=(?P<c3>[A-Za-z_$][A-Za-z0-9_$]*)\[1\]"#;
const BACKREF: &str = r#"(?P<c>[A-Za-z_$][A-Za-z0-9_$]*)\[0\]!==(?P<x>[A-Za-z_$][A-Za-z0-9_$]*)\?\((?P<t>[A-Za-z_$][A-Za-z0-9_$]*)=(?P=x)\?\.get\("enable_i18n",!1\),(?P=c)\[0\]=(?P=x),(?P=c)\[1\]=(?P=t)\):(?P=t)=(?P=c)\[1\]"#;

fn checked_matches(re: &ByteRegex, input: &[u8]) -> Vec<(usize, usize)> {
    let mut positions = Vec::new();
    let mut start = 0;
    while let Some(c) = re.captures_at(input, start) {
        let whole = c.get(0).unwrap();
        let equal = |a, b| c.name(a).unwrap().as_bytes() == c.name(b).unwrap().as_bytes();
        if ["c1", "c2", "c3"].into_iter().all(|n| equal("c0", n))
            && ["x1", "x2"].into_iter().all(|n| equal("x0", n))
            && ["t1", "t2"].into_iter().all(|n| equal("t0", n))
        {
            positions.push((whole.start(), whole.end()));
            start = whole.end();
        } else {
            // A rejected candidate may overlap a valid match. Search from the
            // next byte instead of skipping past the whole rejected candidate.
            start = whole.start() + 1;
        }
    }
    positions
}

fn fancy_matches(re: &FancyRegex, input: &str) -> Result<Vec<(usize, usize)>, fancy_regex::Error> {
    re.find_iter(input)
        .map(|result| result.map(|m| (m.start(), m.end())))
        .collect()
}

fn walk<'a>(node: &'a Value, path: String, result: &mut Vec<(String, &'a Value)>) {
    if let Some(files) = node.get("files").and_then(Value::as_object) {
        for (name, item) in files {
            let child = format!("{path}/{name}");
            if item.get("files").is_some() {
                walk(item, child, result);
            } else if child.starts_with("/webview/")
                && child.ends_with(".js")
                && item.get("offset").is_some()
            {
                result.push((child, item));
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1).ok_or("需要提供只读 ASAR 路径")?;
    let mut file = File::open(path)?;
    let mut head = [0; 16];
    file.read_exact(&mut head)?;
    let word = |i| u32::from_le_bytes(head[i..i + 4].try_into().unwrap());
    let (magic, header_size, length) = (word(0), word(4) as u64, word(12) as usize);
    if magic != 4 || length > 100_000_000 || header_size < length as u64 + 8 {
        return Err("不支持的 ASAR 头部".into());
    }
    let mut header = vec![0; length];
    file.read_exact(&mut header)?;
    let tree: Value = serde_json::from_slice(&header)?;
    let mut entries = Vec::new();
    walk(&tree, String::new(), &mut entries);
    let plain = ByteRegex::new(CANDIDATE)?;
    let fancy = FancyRegex::new(BACKREF)?;
    let raised = fancy_regex::RegexBuilder::new(BACKREF)
        .backtrack_limit(10_000_000)
        .build()?;
    let (mut checked_time, mut fancy_time, mut raised_time) =
        (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let (
        mut examined,
        mut hits,
        mut unsupported,
        mut default_errors,
        mut raised_errors,
        mut snippet_verified,
    ) = (0, 0, 0, 0, 0, 0);
    for (name, item) in entries {
        let offset: u64 = item["offset"].as_str().ok_or("ASAR offset 缺失")?.parse()?;
        let size = item["size"].as_u64().ok_or("ASAR size 缺失")?;
        file.seek(SeekFrom::Start(8 + header_size + offset))?;
        let mut bytes = vec![0; usize::try_from(size)?];
        file.read_exact(&mut bytes)?;
        if !bytes.windows(11).any(|window| window == b"enable_i18n") {
            continue;
        }
        examined += 1;
        let timer = Instant::now();
        let checked = checked_matches(&plain, &bytes);
        checked_time += timer.elapsed();
        if !checked.is_empty() {
            println!("匹配资源：{name}；匹配次数：{}", checked.len());
            hits += checked.len();
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            unsupported += 1;
            continue;
        };
        let timer = Instant::now();
        let backref = fancy_matches(&fancy, text);
        fancy_time += timer.elapsed();
        let backref = match backref {
            Ok(matches) => Some(matches),
            Err(err) => {
                default_errors += 1;
                println!("默认回溯限制失败：{name}；{err}");
                let timer = Instant::now();
                let retried = fancy_matches(&raised, text);
                raised_time += timer.elapsed();
                match retried {
                    Ok(matches) => Some(matches),
                    Err(err) => {
                        raised_errors += 1;
                        println!("提高限制后仍失败：{name}；{err}");
                        None
                    }
                }
            }
        };
        if let Some(backref) = backref {
            if checked != backref {
                return Err(format!("两种算法结果不一致: {name}").into());
            }
        } else {
            // This demonstrates behavior on each already-found match only;
            // it does not prove fancy-regex searched the complete file.
            for &(start, end) in &checked {
                let snippet = &text[start..end];
                if fancy_matches(&fancy, snippet)? != [(0, snippet.len())] {
                    return Err(format!("反向引用未匹配已找到的片段: {name}").into());
                }
                snippet_verified += 1;
            }
        }
    }
    println!("待检查 JS 文件数：{examined}；非 UTF-8 文件数：{unsupported}；总匹配数：{hits}");
    println!(
        "匹配耗时（不含读取和编译表达式）：regex + 校验 {:?}；fancy-regex {:?}",
        checked_time, fancy_time
    );
    println!(
        "fancy-regex 默认上限失败：{default_errors}；提高至 1000 万后失败：{raised_errors}；重试耗时：{raised_time:?}；已验证匹配片段：{snippet_verified}"
    );
    if hits != 1 || unsupported != 0 || raised_errors != 0 {
        return Err("未能确认唯一匹配或存在无法比较的文件".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compare(input: &str, count: usize) {
        let checked = checked_matches(&ByteRegex::new(CANDIDATE).unwrap(), input.as_bytes());
        let fancy = fancy_matches(&FancyRegex::new(BACKREF).unwrap(), input).unwrap();
        assert_eq!(checked, fancy);
        assert_eq!(checked.len(), count);
    }

    #[test]
    fn valid_and_multiple_matches() {
        let good = "A[0]!==B?(C=B?.get(\"enable_i18n\",!1),A[0]=B,A[1]=C):C=A[1]";
        compare(good, 1);
        compare(&format!("中文{good};{good}"), 2);
    }

    #[test]
    fn rejects_inconsistent_variables() {
        let cases = [
            "A[0]!==B?(C=D?.get(\"enable_i18n\",!1),A[0]=B,A[1]=C):C=A[1]",
            "A[0]!==B?(C=B?.get(\"enable_i18n\",!1),D[0]=B,A[1]=C):C=A[1]",
            "A[0]!==B?(C=B?.get(\"enable_i18n\",!1),A[0]=D,A[1]=C):C=A[1]",
            "A[0]!==B?(C=B?.get(\"enable_i18n\",!1),A[0]=B,D[1]=C):C=A[1]",
            "A[0]!==B?(C=B?.get(\"enable_i18n\",!1),A[0]=B,A[1]=D):C=A[1]",
            "A[0]!==B?(C=B?.get(\"enable_i18n\",!1),A[0]=B,A[1]=C):D=A[1]",
            "A[0]!==B?(C=B?.get(\"enable_i18n\",!1),A[0]=B,A[1]=C):C=D[1]",
        ];
        for case in cases {
            compare(case, 0);
        }
    }

    #[test]
    fn overlapping_candidate_and_non_utf8() {
        let valid = "A[0]!==B?(C=B?.get(\"enable_i18n\",!1),A[0]=B,A[1]=C):C=A[1]";
        compare(&format!("X{valid}"), 1);
        let mut bytes = vec![0xff];
        bytes.extend_from_slice(valid.as_bytes());
        assert_eq!(
            checked_matches(&ByteRegex::new(CANDIDATE).unwrap(), &bytes).len(),
            1
        );
        assert!(std::str::from_utf8(&bytes).is_err());
    }
}

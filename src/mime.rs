//! MIME 推断：先看文件头的魔数，再看扩展名，最后判断是不是 UTF-8 文本；都不是就 application/octet-stream。
//! 这是给 File 建上传时申报类型用的，也决定浏览器怎么打开它，所以常见的办公、数据、图片、压缩格式要认全；
//! 不引入第三方类型库：这里的表就是全部约定，缺的类型加在这里。

/// 推断需要的文件头长度（字节）：所有魔数都在前 512 字节内，文本判断也只看这一段。
pub const HEAD_BYTES: usize = 512;
/// 推断失败时的类型。
pub const OCTET_STREAM: &str = "application/octet-stream";

/// 由文件头与文件名推断 MIME。`head` 是文件开头至多 HEAD_BYTES 字节（可短于文件）。
pub fn guess(head: &[u8], name: &str) -> &'static str {
    let ext = extension(name);
    if let Some(mime) = sniff(head, &ext) {
        return mime;
    }
    if let Some(mime) = by_extension(&ext) {
        return mime;
    }
    if looks_like_text(head) {
        return text_kind(head);
    }
    OCTET_STREAM
}

/// 文件名的扩展名（小写、不含点）；没有扩展名时为空。
fn extension(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rfind('.') {
        Some(i) if i > 0 => base[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// 魔数识别。容器格式（zip、OLE2）本身说不清是哪种办公文档，由扩展名细分。
fn sniff(head: &[u8], ext: &str) -> Option<&'static str> {
    let starts = |sig: &[u8]| head.len() >= sig.len() && &head[..sig.len()] == sig;
    if starts(b"%PDF") {
        return Some("application/pdf");
    }
    if starts(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if starts(b"\xFF\xD8\xFF") {
        return Some("image/jpeg");
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return Some("image/gif");
    }
    if starts(b"RIFF") && head.len() >= 12 {
        return match &head[8..12] {
            b"WEBP" => Some("image/webp"),
            b"WAVE" => Some("audio/wav"),
            _ => None,
        };
    }
    if starts(b"PK\x03\x04") {
        return Some(match ext {
            "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "xlsm" => "application/vnd.ms-excel.sheet.macroEnabled.12",
            "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "odt" => "application/vnd.oasis.opendocument.text",
            "ods" => "application/vnd.oasis.opendocument.spreadsheet",
            "odp" => "application/vnd.oasis.opendocument.presentation",
            "epub" => "application/epub+zip",
            "jar" => "application/java-archive",
            "whl" => "application/x-wheel+zip",
            "ipa" | "apk" | "xpi" => "application/zip",
            _ => "application/zip",
        });
    }
    if starts(b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1") {
        return Some(match ext {
            "doc" => "application/msword",
            "xls" => "application/vnd.ms-excel",
            "ppt" => "application/vnd.ms-powerpoint",
            _ => "application/x-ole-storage",
        });
    }
    if starts(b"\x1F\x8B") {
        return Some("application/gzip");
    }
    if starts(b"BZh") {
        return Some("application/x-bzip2");
    }
    if starts(b"\xFD7zXZ\x00") {
        return Some("application/x-xz");
    }
    if starts(b"7z\xBC\xAF\x27\x1C") {
        return Some("application/x-7z-compressed");
    }
    if starts(b"Rar!\x1A\x07") {
        return Some("application/vnd.rar");
    }
    if starts(b"\x28\xB5\x2F\xFD") {
        return Some("application/zstd");
    }
    if starts(b"II*\x00") || starts(b"MM\x00*") {
        return Some("image/tiff");
    }
    if starts(b"ID3") {
        return Some("audio/mpeg");
    }
    if starts(b"OggS") {
        return Some("audio/ogg");
    }
    if starts(b"fLaC") {
        return Some("audio/flac");
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        return Some(if &head[8..11] == b"M4A" { "audio/mp4" } else { "video/mp4" });
    }
    if starts(b"\x1A\x45\xDF\xA3") {
        return Some(if ext == "webm" { "video/webm" } else { "video/x-matroska" });
    }
    if starts(b"SQLite format 3\x00") {
        return Some("application/vnd.sqlite3");
    }
    if starts(b"PAR1") {
        return Some("application/vnd.apache.parquet");
    }
    if starts(b"{\\rtf") {
        return Some("application/rtf");
    }
    if starts(b"%!PS") {
        return Some("application/postscript");
    }
    if starts(b"\x7FELF") {
        return Some("application/x-executable");
    }
    if starts(b"\x00asm") {
        return Some("application/wasm");
    }
    if starts(b"wOF2") {
        return Some("font/woff2");
    }
    if starts(b"wOFF") {
        return Some("font/woff");
    }
    None
}

/// 扩展名表：文本与数据格式靠它（它们没有可靠魔数），图片与压缩格式在魔数认不出时兜底。
fn by_extension(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "txt" | "log" | "ini" | "cfg" | "conf" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "json" => "application/json",
        "jsonl" | "ndjson" => "application/x-ndjson",
        "ipynb" => "application/x-ipynb+json",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "py" => "text/x-python",
        "rs" => "text/x-rust",
        "sh" | "bash" => "application/x-sh",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "sql" => "application/sql",
        "tex" => "application/x-tex",
        "rtf" => "application/rtf",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/vnd.microsoft.icon",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "avif" => "image/avif",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        "zst" => "application/zstd",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "tar" => "application/x-tar",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsm" => "application/vnd.ms-excel.sheet.macroEnabled.12",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "epub" => "application/epub+zip",
        "parquet" => "application/vnd.apache.parquet",
        "feather" | "arrow" => "application/vnd.apache.arrow.file",
        "sqlite" | "db" => "application/vnd.sqlite3",
        "wasm" => "application/wasm",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => return None,
    })
}

/// 文件头是不是 UTF-8 文本：没有 NUL，且除了末尾可能被截断的一个多字节字符外都是合法 UTF-8。空文件也当文本。
fn looks_like_text(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && head.len() - e.valid_up_to() <= 3,
    }
}

/// 没有扩展名的文本按开头几个字符细分：SVG、HTML、XML，其余 text/plain。
fn text_kind(head: &[u8]) -> &'static str {
    let text = String::from_utf8_lossy(head).trim_start_matches('\u{feff}').trim_start().to_ascii_lowercase();
    if text.starts_with("<svg") || (text.starts_with("<?xml") && text.contains("<svg")) {
        "image/svg+xml"
    } else if text.starts_with("<!doctype html") || text.starts_with("<html") {
        "text/html"
    } else if text.starts_with("<?xml") {
        "application/xml"
    } else {
        "text/plain"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 魔数优先于扩展名并细分容器格式() {
        assert_eq!(guess(b"%PDF-1.7 ...", "report.txt"), "application/pdf");
        assert_eq!(guess(b"\x89PNG\r\n\x1a\n....", "noext"), "image/png");
        assert_eq!(guess(b"PK\x03\x04rest", "a.xlsx"), "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet");
        assert_eq!(guess(b"PK\x03\x04rest", "a.docx"), "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
        assert_eq!(guess(b"PK\x03\x04rest", "a.bin"), "application/zip");
        assert_eq!(guess(b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1....", "old.xls"), "application/vnd.ms-excel");
        assert_eq!(guess(b"RIFF\x00\x00\x00\x00WEBPVP8 ", "x"), "image/webp");
        assert_eq!(guess(b"\x00\x00\x00\x18ftypisom", "clip"), "video/mp4");
        assert_eq!(guess(b"PAR1....", "data"), "application/vnd.apache.parquet");
    }

    #[test]
    fn 文本格式靠扩展名_无扩展名时看内容() {
        assert_eq!(guess(b"a,b,c\n1,2,3\n", "table.csv"), "text/csv");
        assert_eq!(guess(b"{\"a\":1}", "x.json"), "application/json");
        assert_eq!(guess(b"# title\n", "README.md"), "text/markdown");
        assert_eq!(guess(b"print(1)\n", "in/dir.with.dots/script.py"), "text/x-python");
        assert_eq!(guess(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>", "pic"), "image/svg+xml");
        assert_eq!(guess(b"<!DOCTYPE html><html>", "page"), "text/html");
        assert_eq!(guess("中文内容".as_bytes(), "note"), "text/plain");
        // 末尾被截断的多字节字符不算二进制。
        let cut = &"中文内容".as_bytes()[..7];
        assert_eq!(guess(cut, "note"), "text/plain");
        assert_eq!(guess(b"", "empty"), "text/plain");
        assert_eq!(guess(b"\x00\x01\x02\x03", "blob"), OCTET_STREAM);
        assert_eq!(guess(b"\x00\x01\x02\x03", ".hidden"), OCTET_STREAM, "点开头的隐藏文件没有扩展名");
    }
}

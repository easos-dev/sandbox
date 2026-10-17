//! 密钥核对：`Authorization: Bearer <密钥>`。比较用常量时间，避免靠响应时间逐字节猜密钥。

use http::HeaderValue;

/// 请求头里的 Bearer 密钥是否等于配置的密钥。
pub fn authorized(header: Option<&HeaderValue>, key: &str) -> bool {
    let Some(value) = header.and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer ")) else {
        return false;
    };
    constant_time_eq(token.trim().as_bytes(), key.as_bytes())
}

/// 长度不同直接为假（长度本身不是秘密）；长度相同则逐字节异或后取或，不提前返回。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 只认完整相等的_bearer_密钥() {
        let key = "s3cret-key";
        let ok = HeaderValue::from_static("Bearer s3cret-key");
        assert!(authorized(Some(&ok), key));
        let wrong = HeaderValue::from_static("Bearer s3cret-kez");
        assert!(!authorized(Some(&wrong), key));
        let prefix = HeaderValue::from_static("Bearer s3cret");
        assert!(!authorized(Some(&prefix), key));
        let basic = HeaderValue::from_static("Basic s3cret-key");
        assert!(!authorized(Some(&basic), key));
        assert!(!authorized(None, key));
    }
}

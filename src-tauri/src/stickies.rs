//! 便签路径与 ID；不访问文件系统。
use std::hash::{BuildHasher, Hasher};

use chrono::NaiveDate;

pub fn rel_dir(date: NaiveDate) -> String {
    format!("stickies/{}", crate::repo::fmt_date(date))
}

pub fn rel_path(date: NaiveDate, id: &str) -> String {
    format!("{}/{}.md", rel_dir(date), id)
}

pub fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 22
        && bytes[..13].iter().all(u8::is_ascii_digit)
        && bytes[13] == b'-'
        && bytes[14..].iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

pub fn new_id() -> String {
    let millis = chrono::Utc::now().timestamp_millis();
    let suffix = std::collections::hash_map::RandomState::new().build_hasher().finish() as u32;
    format!("{millis:013}-{suffix:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_format_and_invalid_ids() {
        let id = new_id();
        assert_eq!(id.len(), 22);
        assert!(valid_id(&id));
        for bad in ["../1727687133123-3fa2b91c", "1727687133123-3FA2B91C", "172768713312-3fa2b91c", "1727687133123-3fa2b91"] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn date_path_is_stable() {
        let day = NaiveDate::from_ymd_opt(2024, 9, 30).unwrap();
        assert_eq!(rel_path(day, "1727687133123-3fa2b91c"), "stickies/2024-09-30/1727687133123-3fa2b91c.md");
    }
}

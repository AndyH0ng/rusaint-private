//! 강의평 본문은 간단한 HTML 로 옵니다. 태그를 지우고 기본 엔티티만 풀어 평문으로 만듭니다.

/// `<br>` 는 줄바꿈으로, 그 밖의 태그는 제거하고, 기본 엔티티를 복원합니다.
pub fn to_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                // 태그 이름을 모아 <br> 만 줄바꿈으로 바꿉니다.
                let mut tag = String::new();
                for t in chars.by_ref() {
                    if t == '>' {
                        break;
                    }
                    tag.push(t);
                }
                let name = tag.trim().trim_start_matches('/').to_ascii_lowercase();
                if name.starts_with("br") || name.starts_with("p") || name.starts_with("div") {
                    out.push('\n');
                }
            }
            _ => out.push(c),
        }
    }
    let out = out
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_and_unescapes() {
        assert_eq!(
            to_text("과제 많음<br>근데 &quot;꿀강&quot; &amp; 좋음"),
            "과제 많음\n근데 \"꿀강\" & 좋음"
        );
    }
}

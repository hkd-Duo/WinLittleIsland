//! 验证码提取（v0.5.5）。
//!
//! 加权打分而不是单条正则：单正则会把你订单号、金额、电话当验证码。
//! 规则参考 pulse-mail（MIT，`otp/mod.rs`）的强/弱/负向词表与形状权重，
//! 中文关键词表按国内邮件场景补齐；门槛 65 分——宁可漏，不可错。

use regex::Regex;
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Code {
    pub text: String,
    pub score: i32,
}

/// 上岛门槛：低于这个分数只当作普通邮件。
pub const THRESHOLD: i32 = 65;

fn strong() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)验证码|校验码|确认码|动态码|动态密码|安全码|登录码|激活码|",
            r"一次性(?:密码|口令)|otp|one[-\s]?time\s+(?:code|password|passcode)|",
            r"verification\s+code|security\s+code|auth(?:entication)?\s+code|",
            r"passcode|\bcode\b|認証コード|인증번호"
        ))
        .expect("strong keyword regex")
    })
}

fn weak() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)登录|登陆|验证|校验|身份|确认|安全|",
            r"sign[-\s]?in|log[-\s]?in|verify|verification|confirm|authenticate"
        ))
        .expect("weak keyword regex")
    })
}

fn candidate() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // 字母前缀、带分隔的 3-4 位分组、4-8 位数字、4-8 位字母数字。
        Regex::new(concat!(
            r"(?:[A-Za-z]{1,3}-)?",
            r"(?:\d{3,4}[- ]\d{3,4}|\d{4}-\d{4}|[A-Za-z0-9]{3,4}-[A-Za-z0-9]{3,4}|\d{4,8}|[A-Za-z0-9]{4,8})"
        ))
        .expect("candidate regex")
    })
}

fn clock() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\d{1,2}:\d{2}").expect("time regex"))
}

/// 全角转半角、去零宽字符、全角空格归一。
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\u{2060}' => continue,
            '\u{3000}' => out.push(' '),
            '\u{ff01}'..='\u{ff5e}' => {
                out.push(char::from_u32(c as u32 - 0xfee0).unwrap_or(c));
            }
            _ => out.push(c),
        }
    }
    out
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn before(text: &str, at: usize, n: usize) -> String {
    let mut v: Vec<char> = text[..at].chars().rev().take(n).collect();
    v.reverse();
    v.into_iter().collect()
}

fn after(text: &str, at: usize, n: usize) -> String {
    text[at..].chars().take(n).collect()
}

fn around(text: &str, start: usize, end: usize, n: usize) -> String {
    format!("{}{}", before(text, start, n), after(text, end, n))
}

fn has(win: &str, words: &[&str]) -> bool {
    let lower = win.to_ascii_lowercase();
    words.iter().any(|w| lower.contains(w))
}

const ORDER_WORDS: &[&str] = &[
    "订单", "单号", "快递", "运单", "物流", "包裹", "编号", "序号", "发票", "金额", "合计",
    "实付", "应付", "电话", "手机号", "会议号", "会议", "卡尾号", "尾号", "邮编", "sku",
    "order", "track", "invoice", "amount", "phone", "meeting", "card ending",
];

const MONEY_WORDS: &[&str] = &["¥", "￥", "$", "€", "元", "价格", "金额", "合计", "实付", "应付"];

const PROMO_WORDS: &[&str] = &[
    "优惠", "折扣", "促销", "满减", "券", "红包", "coupon", "promo", "discount", "sale",
];

const EXPIRE_WORDS: &[&str] = &[
    "有效", "过期", "失效", "expire", "valid", "minutes", "分钟", "小时内",
];

/// 从主题 + 正文里挑出最像验证码的一个；`emphasized` 是 HTML 里加粗/大字号片段的纯文本。
pub fn extract(subject: &str, body: &str, emphasized: &[String]) -> Option<Code> {
    let subject = normalize(subject);
    let body = normalize(&take_chars(body, 6000));
    let hay = format!("{subject}\n{body}");
    let subject_end = subject.chars().count() + 1;
    let emph: Vec<String> = emphasized.iter().map(|s| normalize(s)).collect();

    let mut best: Option<(i32, usize, bool, Code)> = None;
    for m in candidate().find_iter(&hay) {
        let prev = hay[..m.start()].chars().next_back();
        let next = hay[m.end()..].chars().next();
        // 只认 ASCII 字母数字边界：中文紧贴验证码是常态，不能当边界外丢弃。
        if prev.is_some_and(|c| c.is_ascii_alphanumeric()) || next.is_some_and(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        let text = m.as_str().to_string();
        let Some((score, strong_signal)) = score_at(&hay, m.start(), m.end(), &text, subject_end, &emph) else {
            continue;
        };
        // 没有强信号（强关键词 / 主题 / HTML 强调 / 独占行 / 冒号后）就不算验证码：
        // 宁可不显示，也不能把订单号、金额之类的数字放大展示。
        if !strong_signal {
            continue;
        }
        let char_pos = hay[..m.start()].chars().count();
        let in_subject = char_pos < subject_end;
        let better = match &best {
            None => true,
            Some((s, p, subj, _)) => {
                score > *s
                    || (score == *s && in_subject && !*subj)
                    || (score == *s && in_subject == *subj && char_pos < *p)
            }
        };
        if better {
            best = Some((score, char_pos, in_subject, Code { text, score }));
        }
    }
    best.map(|(_, _, _, code)| code).filter(|c| c.score >= THRESHOLD)
}

fn score_at(hay: &str, start: usize, end: usize, text: &str, subject_end: usize, emph: &[String]) -> Option<(i32, bool)> {
    let core: String = text.chars().filter(|c| c.is_alphanumeric()).collect();
    let digits = core.chars().filter(|c| c.is_ascii_digit()).count();
    let letters = core.chars().filter(|c| c.is_ascii_alphabetic()).count();
    if digits == 0 {
        return None;
    }
    let mixed = letters > 0;
    let separated = text.chars().any(|c| c == '-' || c == ' ');
    let len = core.chars().count();

    let mut score = if mixed {
        if len < 5 {
            return None;
        }
        if len == 6 {
            20
        } else if separated {
            22
        } else {
            14
        }
    } else {
        match len {
            6 => {
                if separated {
                    28
                } else {
                    30
                }
            }
            7 | 8 => {
                if separated {
                    22
                } else {
                    18
                }
            }
            5 => 20,
            4 => 16,
            _ => return None,
        }
    };

    let b40 = before(hay, start, 40);
    let b250 = before(hay, start, 250);
    let b60 = before(hay, start, 60);
    let a40 = after(hay, end, 40);

    // ---- 硬丢弃：URL / 邮箱 / 日期时间 / 电话 / 订单金额卡号 ----
    let token = {
        let left = hay[..start].char_indices().rev()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8()).unwrap_or(0);
        let right = hay[end..].find(char::is_whitespace).map(|i| end + i).unwrap_or(hay.len());
        &hay[left..right]
    };
    let tl = token.to_ascii_lowercase();
    if tl.contains("http") || tl.contains("www.") || token.contains('/') || token.contains('@') {
        return None;
    }
    let date_win = around(hay, start, end, 6);
    if has(&date_win, &["年", "月", "日", "有效期至"])
        || clock().is_match(&date_win)
        || clock().is_match(&before(hay, start, 8))
    {
        return None;
    }
    if has(&around(hay, start, end, 6), MONEY_WORDS) {
        return None;
    }
    // 订单/电话/会议号这类词只认紧邻的前文与极短的后续：真实验证码后面常跟
    // “如非本人操作…订单号…”这类无关尾巴，窗口放宽会把真码误杀。
    if has(&before(hay, start, 20), ORDER_WORDS) || has(&after(hay, end, 8), ORDER_WORDS) {
        return None;
    }
    // 4 位年份：直接判负，掉到门槛以下
    if len == 4 && !mixed {
        if let Ok(y) = core.parse::<u32>() {
            if (1900..=2100).contains(&y) {
                score -= 35;
            }
        }
    }

    // ---- 正向信号 ----
    let strong_before = strong().is_match(&b40);
    if strong_before {
        score += 40;
    } else if strong().is_match(&b250) {
        score += 22;
    } else if weak().is_match(&b60) {
        score += 15;
    }
    if strong().is_match(&a40) {
        score += 28;
    }

    let line_start = hay[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = hay[start..].find('\n').map(|i| start + i).unwrap_or(hay.len());
    let line = hay[line_start..line_end]
        .trim()
        .trim_end_matches(['。', '，', ',', '.', '！', '!', '、', ';', '；'])
        .trim();
    let own_line = line == text;
    let colon = hay[..start].trim_end().ends_with(':');
    let char_pos = hay[..start].chars().count();
    let in_subject = char_pos < subject_end;
    let is_emph = emph.iter().any(|e| e.contains(text));

    if mixed && !(own_line || colon || in_subject || is_emph) {
        return None;
    }

    if own_line {
        score += 15;
    }
    if colon {
        score += 10;
    }
    if in_subject {
        score += 15;
    }
    if is_emph {
        score += 25;
    }
    if has(&b40, &["验证码是", "验证码为", "校验码是", "code is", "code:"]) {
        score += 5;
    }
    if has(&a40, EXPIRE_WORDS) || has(&b60, EXPIRE_WORDS) {
        score += 5;
    }
    if has(&around(hay, start, end, 40), PROMO_WORDS) {
        score -= 30;
    }
    // 强信号：任何一个成立才允许上岛展示。
    let strong_signal = strong_before || strong().is_match(&a40) || in_subject || is_emph || own_line || colon;
    Some((score, strong_signal))
}

/// HTML → 纯文本：去脚本样式、块级标签转换行、解常见实体、压缩空白。
pub fn html_to_text(html: &str) -> String {
    static SCRIPT: OnceLock<Regex> = OnceLock::new();
    static TAGS: OnceLock<Regex> = OnceLock::new();
    static BREAK: OnceLock<Regex> = OnceLock::new();
    let script = SCRIPT.get_or_init(|| Regex::new(r"(?is)<(?:script|style)[^>]*>.*?</(?:script|style)>").expect("script regex"));
    let tags = TAGS.get_or_init(|| Regex::new(r"(?s)<[^>]+>").expect("tag regex"));
    let br = BREAK.get_or_init(|| {
        Regex::new(r"(?i)<br\s*/?>|</p>|</div>|</tr>|</li>|</h[1-6]>").expect("break regex")
    });
    let s = script.replace_all(html, " ");
    let s = br.replace_all(&s, "\n");
    let s = tags.replace_all(&s, "");
    let s = s
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let t = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !t.is_empty() {
            out.push_str(&t);
            out.push('\n');
        }
    }
    out
}

/// HTML 里被「当码呈现」的片段：加粗、标题、大字号 span。
pub fn emphasized(html: &str) -> Vec<String> {
    static TAGS: OnceLock<Regex> = OnceLock::new();
    let re = TAGS.get_or_init(|| {
        Regex::new(concat!(
            r"(?is)<(?:b|strong|h[1-6])[^>]*>(.*?)</(?:b|strong|h[1-6])>|",
            r#"<span[^>]*font-size:\s*(?:2\d|3\d|4\d)px[^>]*>(.*?)</span>"#
        ))
        .expect("emphasis regex")
    });
    re.captures_iter(html)
        .filter_map(|c| c.get(1).or_else(|| c.get(2)))
        .map(|m| html_to_text(m.as_str()).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(subject: &str, body: &str) -> Option<String> {
        extract(subject, body, &[]).map(|c| c.text)
    }

    #[test]
    fn chinese_codes_with_strong_keyword() {
        assert_eq!(code("【某某】验证码", "您的验证码是：834921，10分钟内有效。"), Some("834921".into()));
        assert_eq!(code("", "动态密码：482913（5分钟内有效）"), Some("482913".into()));
        assert_eq!(code("", "验证码 906213"), Some("906213".into()));
        assert_eq!(code("", "安全码：4021"), Some("4021".into()));
        assert_eq!(code("", "验证码\n735164\n请勿泄露"), Some("735164".into()));
    }

    #[test]
    fn unicode_whitespace_before_code_is_safe() {
        for space in ['\u{00a0}', '\u{2002}', '\u{2003}', '\u{202f}', '\u{3000}'] {
            let text = format!("验证码：{space}834921");
            assert_eq!(code("", &text), Some("834921".into()));
            assert_eq!(code(&text, ""), Some("834921".into()));
            assert_eq!(code("", &format!("验证码：{space}https://example.com/834921")), None);
        }
    }

    #[test]
    fn english_and_mixed_alnum_codes() {
        assert_eq!(
            code("Your verification code", "Your verification code is 552310. It expires in 10 minutes."),
            Some("552310".into())
        );
        assert_eq!(code("", "验证码：A1B2C3"), Some("A1B2C3".into()));
        assert_eq!(code("验证码 K7Q2-9XMP", "10 分钟内有效"), Some("K7Q2-9XMP".into()));
    }

    #[test]
    fn full_width_digits_are_normalized() {
        assert_eq!(code("", "您的验证码是：８３４９２１，10分钟内有效"), Some("834921".into()));
    }

    #[test]
    fn emphasized_html_without_nearby_keyword() {
        let body = html_to_text("<p>尊敬的用户，您正在进行登录操作。</p><p>本次校验：<b>834921</b></p>");
        let emph = emphasized("<p>尊敬的用户，您正在进行登录操作。</p><p>本次校验：<b>834921</b></p>");
        assert_eq!(extract("", &body, &emph).map(|c| c.text), Some("834921".into()));
    }

    #[test]
    fn negatives_are_rejected() {
        let cases = [
            ("", "您的订单号 12345678 已发货"),
            ("", "订单 482913，金额 ￥1,299.00"),
            ("", "会议号 123 456 789"),
            ("", "2026年10月30日 13:20 开始"),
            ("", "客服电话 400-800-8888"),
            ("", "优惠券码 SAVE20，立减 20 元"),
            ("", "https://example.com/verify/482913"),
            ("", "卡尾号 8891"),
            ("", "邮编 200120"),
            ("", "验证码有效期至 2026-10-30"),
            ("", "产品型号 SKU-8891 已出库"),
            ("", "验证码请见图片"),
            ("", "您的快递 773456789012 已到驿站"),
            ("", "验证码：请在邮箱中查看"),
        ];
        for (subject, body) in cases {
            assert_eq!(code(subject, body), None, "should reject: {body}");
        }
    }

    #[test]
    fn order_number_does_not_beat_real_code() {
        assert_eq!(
            code("", "验证码：834921，如非本人操作请忽略。订单号 987654"),
            Some("834921".into())
        );
    }

    #[test]
    fn html_to_text_strips_tags_and_entities() {
        let text = html_to_text("<div>验证码&nbsp;123456</div><script>var x=1</script>");
        assert!(text.contains("验证码 123456"));
        assert!(!text.contains("var x"));
    }
}

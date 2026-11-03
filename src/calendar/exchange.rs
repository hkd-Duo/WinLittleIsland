//! 企业微信 Exchange ActiveSync 14.0：只读取日历，不发送日程修改或设备管理命令。
use super::{Config, Failure, Fetched, Provider};
use chrono::{DateTime, Utc};
use std::io::Read;

// MS-ASWBXML 的 AirSync、Calendar、FolderHierarchy 代码页。
const AIR: u8 = 0;
const CAL: u8 = 4;
const FOLDER: u8 = 7;

#[derive(Clone, Debug, Default)]
struct Node { page: u8, tag: u8, value: String, children: Vec<Node> }
impl Node {
    fn child(&self, page: u8, tag: u8) -> Option<&Node> { self.children.iter().find(|n| n.page == page && n.tag == tag) }
    fn text(&self, page: u8, tag: u8) -> &str { self.child(page, tag).map_or("", |n| n.value.as_str()) }
    fn required(&self, page: u8, tag: u8) -> Result<&str, Failure> {
        let value = self.text(page, tag);
        if value.is_empty() { Err(invalid()) } else { Ok(value) }
    }
}
fn invalid() -> Failure { Failure::text("企业微信返回的日历数据不完整，请稍后重试") }
fn node(page: u8, tag: u8, value: &str, children: Vec<Node>) -> Node {
    Node { page, tag, value: value.into(), children }
}
fn encode(root: &Node) -> Vec<u8> {
    fn write(n: &Node, page: &mut u8, out: &mut Vec<u8>) {
        if *page != n.page { out.extend([0, n.page]); *page = n.page; }
        let content = !n.value.is_empty() || !n.children.is_empty();
        out.push(n.tag | if content { 0x40 } else { 0 });
        if !n.value.is_empty() { out.push(3); out.extend(n.value.as_bytes()); out.push(0); }
        for child in &n.children { write(child, page, out); }
        if content { out.push(1); }
    }
    let mut bytes = vec![3, 1, 106, 0];
    write(root, &mut 0, &mut bytes);
    bytes
}
fn decode(bytes: &[u8]) -> Result<Node, Failure> {
    fn integer(bytes: &[u8], at: &mut usize) -> Result<usize, Failure> {
        let mut value = 0usize;
        for _ in 0..5 {
            let b = *bytes.get(*at).ok_or_else(invalid)?; *at += 1;
            value = value.checked_mul(128).and_then(|v| v.checked_add((b & 127) as usize)).ok_or_else(invalid)?;
            if b & 128 == 0 { return Ok(value); }
        }
        Err(invalid())
    }
    let mut at = 1;
    if bytes.first() != Some(&3) || integer(bytes, &mut at)? != 1 || integer(bytes, &mut at)? != 106 || integer(bytes, &mut at)? != 0 { return Err(invalid()); }
    let mut page = 0;
    let mut stack = vec![Node::default()];
    while at < bytes.len() {
        let token = bytes[at]; at += 1;
        match token {
            0 => { page = *bytes.get(at).ok_or_else(invalid)?; at += 1; }
            1 => {
                if stack.len() <= 1 { return Err(invalid()); }
                let n = stack.pop().unwrap(); stack.last_mut().unwrap().children.push(n);
            }
            3 | 0xc3 => {
                if stack.len() <= 1 { return Err(invalid()); }
                let len = if token == 3 { bytes[at..].iter().position(|b| *b == 0).ok_or_else(invalid)? } else { integer(bytes, &mut at)? };
                let end = at.checked_add(len).filter(|end| *end <= bytes.len()).ok_or_else(invalid)?;
                stack.last_mut().unwrap().value.push_str(&String::from_utf8_lossy(&bytes[at..end]));
                at = end + usize::from(token == 3);
            }
            t if t & 0x80 == 0 && t & 0x3f >= 5 => {
                let n = node(page, t & 0x3f, "", vec![]);
                if t & 0x40 != 0 {
                    if stack.len() >= 64 { return Err(invalid()); }
                    stack.push(n);
                } else { stack.last_mut().unwrap().children.push(n); }
            }
            _ => return Err(invalid()),
        }
    }
    if stack.len() != 1 || stack[0].children.len() != 1 { return Err(invalid()); }
    Ok(stack.pop().unwrap().children.remove(0))
}

fn endpoint(cfg: &Config) -> Result<url::Url, Failure> {
    let base = url::Url::parse(&cfg.base()).map_err(|_| invalid())?;
    let local = base.host_str().is_some_and(|h| h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()));
    if !(base.scheme() == "https" || base.scheme() == "http" && local) || !base.username().is_empty() || base.password().is_some() { return Err(Failure::text("日历服务器地址不安全")); }
    base.join("/Microsoft-Server-ActiveSync").map_err(|_| invalid())
}
fn command(agent: &ureq::Agent, cfg: &Config, password: &str, name: &str, root: &Node) -> Result<Node, Failure> {
    let mut target = endpoint(cfg)?;
    // 固定匿名设备标识，完整快照按顺序读取，不保存同步键或日程正文。
    target.query_pairs_mut().extend_pairs([("Cmd", name), ("User", cfg.username.as_str()), ("DeviceId", "DaozaiCalendar01"), ("DeviceType", "Daozai")]);
    let response = agent.post(target.as_str())
        .set("Authorization", &super::basic_auth(&cfg.username, password))
        .set("MS-ASProtocolVersion", "14.0")
        .set("Content-Type", "application/vnd.ms-sync.wbxml")
        .send_bytes(&encode(root)).map_err(|e| match e {
            ureq::Error::Status(code, _) => Failure::status(Provider::WeCom, code),
            _ => Failure::transport("Exchange"),
        })?;
    if response.status() != 200 { return Err(Failure::status(Provider::WeCom, response.status())); }
    let mut bytes = Vec::new();
    response.into_reader().take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|_| Failure::transport("Exchange"))?;
    if bytes.len() > 16 * 1024 * 1024 { return Err(Failure::text("企业微信单次返回的日程过多，无法完整同步")); }
    decode(&bytes)
}
fn check_status(n: &Node, page: u8, tag: u8) -> Result<(), Failure> {
    match n.required(page, tag)? {
        "1" => Ok(()),
        code => Err(Failure::text(format!("企业微信同步失败（Exchange {code}），请检查日历授权后重试"))),
    }
}
fn sync_request(id: &str, key: &str) -> Node {
    let mut fields = vec![node(AIR, 0x0b, key, vec![]), node(AIR, 0x12, id, vec![])];
    if key != "0" {
        fields.extend([node(AIR, 0x13, "1", vec![]), node(AIR, 0x15, "100", vec![]),
            node(AIR, 0x17, "", vec![node(AIR, 0x18, "0", vec![])])]);
    }
    node(AIR, 5, "", vec![node(AIR, 0x1c, "", vec![node(AIR, 0x0f, "", fields)])])
}

pub(super) fn fetch(cfg: &Config, password: &str, lo: DateTime<Utc>, hi: DateTime<Utc>) -> Result<Fetched, Failure> {
    let agent = super::agent();
    let folders = command(&agent, cfg, password, "FolderSync", &node(FOLDER, 0x16, "", vec![node(FOLDER, 0x12, "0", vec![])]))?;
    check_status(&folders, FOLDER, 0x0c)?;
    let changes = folders.child(FOLDER, 0x0e).ok_or_else(invalid)?;
    let mut calendars = 0;
    let mut events = Vec::new();
    for folder in changes.children.iter().filter(|n| n.page == FOLDER && n.tag == 0x0f && matches!(n.text(FOLDER, 0x0a), "8" | "13")) {
        calendars += 1;
        let id = folder.required(FOLDER, 8)?;
        let mut key = "0".to_string();
        let mut items = std::collections::BTreeMap::new();
        for batch in 0..101 {
            if batch == 100 { return Err(Failure::text("企业微信日程数量过多，无法完整同步")); }
            let result = command(&agent, cfg, password, "Sync", &sync_request(id, &key))?;
            if result.child(AIR, 0x0e).is_some() { check_status(&result, AIR, 0x0e)?; }
            let collection = result.child(AIR, 0x1c).and_then(|n| n.child(AIR, 0x0f)).ok_or_else(invalid)?;
            check_status(collection, AIR, 0x0e)?;
            let next = collection.required(AIR, 0x0b)?.to_string();
            if next == "0" || next == key && collection.child(AIR, 0x14).is_some() { return Err(invalid()); }
            if let Some(commands) = collection.child(AIR, 0x16) {
                for change in &commands.children {
                    if change.page != AIR { continue; }
                    let server_id = change.required(AIR, 0x0d)?;
                    match change.tag {
                        7 | 8 => { items.insert(server_id.to_string(), change.child(AIR, 0x1d).ok_or_else(invalid)?.clone()); }
                        9 | 0x21 => { items.remove(server_id); }
                        _ => return Err(invalid()),
                    }
                }
            }
            let initial = key == "0";
            key = next;
            if !initial && collection.child(AIR, 0x14).is_none() { break; }
        }
        let mut ics = String::from("BEGIN:VCALENDAR\nVERSION:2.0\n");
        for (server_id, item) in items { ics.push_str(&event_ics(&item, &format!("wecom:{id}:{server_id}"))?); }
        ics.push_str("END:VCALENDAR");
        let xml = format!("<calendar-data>{}</calendar-data>", ics.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"));
        events.extend(super::events_in_range(&xml, lo, hi)?);
    }
    if calendars == 0 { return Err(Failure::text("企业微信未返回可同步的日历，请确认已开启日程同步")); }
    events.sort_by_key(|m| m.start);
    let mut seen = std::collections::HashSet::new();
    events.retain(|m| seen.insert(super::dedupe_key(m)));
    Ok(Fetched { href: endpoint(cfg)?.to_string(), events })
}

fn date(value: &str) -> Result<DateTime<Utc>, Failure> {
    chrono::NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ").map(|d| d.and_utc()).map_err(|_| invalid())
}
fn escaped(value: &str) -> String { value.replace('\\', "\\\\").replace('\r', "").replace('\n', "\\n").replace(';', "\\;").replace(',', "\\,") }
fn recurrence(n: &Node) -> Result<String, Failure> {
    let kind = n.required(CAL, 0x1c)?;
    let freq = match kind { "0" => "DAILY", "1" => "WEEKLY", "2" | "3" => "MONTHLY", "5" | "6" => "YEARLY", _ => return Err(Failure::text("暂不支持企业微信返回的重复日程类型")) };
    if !matches!(n.text(CAL, 0x37), "" | "0" | "1") { return Err(Failure::text("暂不支持企业微信的非公历重复日程")); }
    let mut rule = format!("RRULE:FREQ={freq}");
    for (tag, name) in [(0x1f, "INTERVAL"), (0x1e, "COUNT"), (0x21, "BYMONTHDAY"), (0x23, "BYMONTH")] {
        if !n.text(CAL, tag).is_empty() {
            let value = n.text(CAL, tag).parse::<u32>().map_err(|_| invalid())?;
            if value == 0 { return Err(invalid()); }
            rule.push_str(&format!(";{name}={value}"));
        }
    }
    if !n.text(CAL, 0x1d).is_empty() { rule.push_str(&format!(";UNTIL={}", date(n.text(CAL, 0x1d))?.format("%Y%m%dT%H%M%SZ"))); }
    if !n.text(CAL, 0x20).is_empty() {
        let bits = n.text(CAL, 0x20).parse::<u8>().map_err(|_| invalid())?;
        let days: Vec<_> = ["SU", "MO", "TU", "WE", "TH", "FR", "SA"].into_iter().enumerate().filter_map(|(i, day)| (bits & (1 << i) != 0).then_some(day)).collect();
        if days.is_empty() { return Err(invalid()); }
        rule.push_str(&format!(";BYDAY={}", days.join(",")));
    }
    if matches!(kind, "3" | "6") {
        let week = n.required(CAL, 0x22)?.parse::<i32>().map_err(|_| invalid())?;
        if !(1..=5).contains(&week) { return Err(invalid()); }
        rule.push_str(&format!(";BYSETPOS={}", if week == 5 { -1 } else { week }));
    }
    rule.push('\n');
    Ok(rule)
}
fn event_ics(item: &Node, fallback_uid: &str) -> Result<String, Failure> {
    if matches!(item.text(CAL, 0x18), "5" | "7") { return Ok(String::new()); }
    let uid = if item.text(CAL, 0x28).is_empty() { fallback_uid } else { item.text(CAL, 0x28) };
    let start = date(item.required(CAL, 0x27)?)?;
    let end = date(item.required(CAL, 0x12)?)?;
    let all_day = item.text(CAL, 6) == "1";
    let timezone = item.text(CAL, 5);
    let mut offset = 0i64;
    if !timezone.is_empty() {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(timezone).map_err(|_| invalid())?;
        if bytes.len() != 172 { return Err(invalid()); }
        let number = |at| i32::from_le_bytes(bytes[at..at+4].try_into().unwrap()) as i64;
        offset = -(number(0) + number(84)) * 60;
        if item.child(CAL, 0x1b).is_some() && bytes[154..156] != [0, 0] {
            return Err(Failure::text("暂不支持企业微信含夏令时切换的重复日程"));
        }
    }
    // 按日历时区展开重复规则，避免 UTC 日期跨日后把周几或月日算错。
    let recurring = item.child(CAL, 0x1b).is_some();
    let recurrence_zone = if recurring && offset != 0 && !all_day {
        if offset % 3600 != 0 || !(-12*3600..=14*3600).contains(&offset) { return Err(Failure::text("暂不支持企业微信此时区的重复日程")); }
        Some(format!("Etc/GMT{:+}", -offset / 3600))
    } else { None };
    let time_line = |name: &str, at: DateTime<Utc>| {
        if all_day { format!("{name};VALUE=DATE:{}\n", (at + chrono::Duration::seconds(offset)).format("%Y%m%d")) }
        else if let Some(zone) = &recurrence_zone { format!("{name};TZID={zone}:{}\n", (at + chrono::Duration::seconds(offset)).format("%Y%m%dT%H%M%S")) }
        else { format!("{name}:{}\n", at.format("%Y%m%dT%H%M%SZ")) }
    };
    let mut result = format!("BEGIN:VEVENT\nUID:{}\nSUMMARY:{}\n{}{}", escaped(uid), escaped(item.text(CAL, 0x26)), time_line("DTSTART", start), time_line("DTEND", end));
    if let Some(rule) = item.child(CAL, 0x1b) { result.push_str(&recurrence(rule)?); }
    result.push_str("END:VEVENT\n");
    if let Some(exceptions) = item.child(CAL, 0x14) {
        for exception in &exceptions.children {
            if exception.page != CAL || exception.tag != 0x13 { continue; }
            let original = date(exception.required(CAL, 0x16)?)?;
            let mut changed = item.clone();
            changed.children.retain(|n| !(n.page == CAL && matches!(n.tag, 0x1b | 0x14)));
            let duration = end - start;
            for (tag, value) in [(0x27, original), (0x12, original + duration)] {
                changed.children.retain(|n| !(n.page == CAL && n.tag == tag));
                changed.children.push(node(CAL, tag, &value.format("%Y%m%dT%H%M%SZ").to_string(), vec![]));
            }
            for field in &exception.children {
                changed.children.retain(|n| !(n.page == field.page && n.tag == field.tag));
                changed.children.push(field.clone());
            }
            let mut instance = event_ics(&changed, uid)?;
            let cancelled = exception.text(CAL, 0x15) == "1" || matches!(exception.text(CAL, 0x18), "5" | "7");
            if cancelled { instance = format!("BEGIN:VEVENT\nUID:{}\n{}STATUS:CANCELLED\nEND:VEVENT\n", escaped(uid), time_line("DTSTART", original)); }
            result.push_str(&instance.replacen("END:VEVENT", &format!("{}END:VEVENT", time_line("RECURRENCE-ID", original)), 1));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Node {
        node(AIR, 0x1d, "", vec![node(CAL, 0x28, "test-uid", vec![]), node(CAL, 0x26, "会议<&>\n第二行", vec![]),
            node(CAL, 0x27, "20261103T020000Z", vec![]), node(CAL, 0x12, "20261103T030000Z", vec![])])
    }
    fn parsed(item: &Node) -> Vec<crate::mail::Meeting> {
        let ics = event_ics(item, "fallback").unwrap();
        let xml = format!("<calendar-data>{}</calendar-data>", ics.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"));
        super::super::events_in_range(&xml, date("20261101T000000Z").unwrap(), date("20261201T000000Z").unwrap()).unwrap()
    }
    #[test]
    fn wbxml_roundtrip_rejects_truncation_and_keeps_namespaces() {
        let root = node(AIR, 5, "", vec![sample(), node(AIR, 0x14, "", vec![])]);
        let bytes = encode(&root);
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.children[0].text(CAL, 0x26), "会议<&>\n第二行");
        assert!(decoded.child(AIR, 0x14).is_some());
        for end in 0..bytes.len() { assert!(decode(&bytes[..end]).is_err()); }
        assert!(decode(&[3,1,106,0,0x45,0xc3,0xff,0xff,0xff,0xff,0xff]).is_err());
    }
    #[test]
    fn calendar_recurring_exceptions_and_all_day_keep_correct_dates() {
        let mut item = sample();
        item.children.push(node(CAL, 0x1b, "", vec![node(CAL, 0x1c, "0", vec![]), node(CAL, 0x1e, "3", vec![])]));
        item.children.push(node(CAL, 0x14, "", vec![
            node(CAL, 0x13, "", vec![node(CAL, 0x16, "20261104T020000Z", vec![]), node(CAL, 0x15, "1", vec![])]),
            node(CAL, 0x13, "", vec![node(CAL, 0x16, "20261105T020000Z", vec![]), node(CAL, 0x27, "20261105T050000Z", vec![]), node(CAL, 0x12, "20261105T060000Z", vec![])])
        ]));
        let events = parsed(&item);
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].start, date("20261105T050000Z").unwrap().timestamp());
        assert_eq!(events[1].end, Some(date("20261105T060000Z").unwrap().timestamp()));
        use base64::Engine;
        let mut timezone = [0u8; 172]; timezone[..4].copy_from_slice(&(-480i32).to_le_bytes());
        let mut all_day = sample();
        all_day.children.retain(|n| !matches!(n.tag, 0x27 | 0x12));
        all_day.children.extend([node(CAL, 6, "1", vec![]), node(CAL, 5, &base64::engine::general_purpose::STANDARD.encode(timezone), vec![]),
            node(CAL, 0x27, "20261102T160000Z", vec![]), node(CAL, 0x12, "20261103T160000Z", vec![])]);
        let ics = event_ics(&all_day, "fallback").unwrap();
        assert!(ics.contains("DTSTART;VALUE=DATE:20261103\n"));
        assert!(ics.contains("DTEND;VALUE=DATE:20261104\n"));
        assert!(parsed(&all_day)[0].all_day);
    }
    #[test]
    fn exchange_fetch_reads_only_calendar_and_follows_every_page() {
        use std::{io::Write, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for index in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
                let mut bytes = Vec::new();
                let mut byte = [0];
                while !bytes.ends_with(b"\r\n\r\n") { stream.read_exact(&mut byte).unwrap(); bytes.push(byte[0]); }
                let header = String::from_utf8(bytes).unwrap();
                let len = header.lines().find_map(|line| line.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap();
                let mut body = vec![0; len]; stream.read_exact(&mut body).unwrap();
                let request = decode(&body).unwrap();
                assert!(header.starts_with("POST /Microsoft-Server-ActiveSync?"));
                let response = if index == 0 {
                    assert_eq!((request.page, request.tag), (FOLDER, 0x16));
                    node(FOLDER, 0x16, "", vec![node(FOLDER, 0x0c, "1", vec![]), node(FOLDER, 0x0e, "", vec![
                        node(FOLDER, 0x0f, "", vec![node(FOLDER, 8, "inbox", vec![]), node(FOLDER, 0x0a, "2", vec![])]),
                        node(FOLDER, 0x0f, "", vec![node(FOLDER, 8, "calendar", vec![]), node(FOLDER, 0x0a, "8", vec![])])])])
                } else {
                    let collection = request.child(AIR, 0x1c).unwrap().child(AIR, 0x0f).unwrap();
                    assert_eq!(collection.text(AIR, 0x12), "calendar");
                    assert_eq!(collection.text(AIR, 0x0b), (index-1).to_string());
                    assert!(collection.child(AIR, 0x16).is_none(), "只读请求不发送修改命令");
                    let mut children = vec![node(AIR, 0x0e, "1", vec![]), node(AIR, 0x0b, &index.to_string(), vec![])];
                    if index == 2 { children.extend([node(AIR, 0x14, "", vec![]), node(AIR, 0x16, "", vec![node(AIR, 7, "", vec![node(AIR, 0x0d, "event", vec![]), sample()])])]); }
                    node(AIR, 5, "", vec![node(AIR, 0x1c, "", vec![node(AIR, 0x0f, "", children)])])
                };
                let body = encode(&response);
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap(); stream.write_all(&body).unwrap();
            }
        });
        let cfg = Config { provider: Provider::WeCom, username: "demo".into(), href: Some("https://caldav.wecom.work/obsolete".into()), server: Some(base) };
        let result = super::super::fetch_month(&cfg, "test", chrono::NaiveDate::from_ymd_opt(2026,11,1).unwrap()).unwrap();
        assert_eq!(result.events.len(), 1);
        server.join().unwrap();
    }
    #[test]
    fn exchange_status_errors_and_unsafe_endpoints_are_rejected() {
        assert!(check_status(&node(FOLDER, 0x16, "", vec![node(FOLDER, 0x0c, "9", vec![])]), FOLDER, 0x0c).is_err());
        assert!(check_status(&Node::default(), AIR, 0x0e).is_err());
        let cfg = Config { provider: Provider::WeCom, username: "demo".into(), href: None, server: Some("http://wecom.work".into()) };
        assert!(endpoint(&cfg).is_err());
    }
    #[test]
    fn calendar_weekly_rule_uses_local_weekday_across_utc_midnight() {
        use base64::Engine;
        let mut timezone = [0u8; 172]; timezone[..4].copy_from_slice(&(-480i32).to_le_bytes());
        let mut item = sample();
        item.children.retain(|n| !matches!(n.tag, 0x27 | 0x12));
        item.children.extend([node(CAL, 5, &base64::engine::general_purpose::STANDARD.encode(timezone), vec![]),
            node(CAL, 0x27, "20261102T170000Z", vec![]), node(CAL, 0x12, "20261102T180000Z", vec![]),
            node(CAL, 0x1b, "", vec![node(CAL, 0x1c, "1", vec![]), node(CAL, 0x20, "4", vec![]), node(CAL, 0x1e, "2", vec![])])]);
        let events = parsed(&item);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].start, date("20261102T170000Z").unwrap().timestamp());
        assert_eq!(events[1].start, date("20261109T170000Z").unwrap().timestamp());
    }
    #[test]
    #[ignore = "显式验证企业微信账号；仅输出数量，成功后可按环境变量保存连接"]
    fn live_wecom_connection() {
        let mut cfg = Config { provider: Provider::WeCom, username: std::env::var("DAOZAI_WECOM_USER").unwrap(), href: None, server: None };
        let password = std::env::var("DAOZAI_WECOM_PASSWORD").unwrap();
        let month = super::super::fetch_month(&cfg, &password, chrono::Local::now().date_naive()).unwrap();
        let upcoming = super::super::fetch(&cfg, &password).unwrap();
        eprintln!("wecom month_events={} upcoming_events={}", month.events.len(), upcoming.events.len());
        if std::env::var("DAOZAI_WECOM_SAVE").as_deref() == Ok("1") {
            cfg.href = Some(month.href);
            super::super::save_connection(&cfg, &password, None).unwrap();
        }
    }
}

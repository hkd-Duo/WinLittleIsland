//! GLM（智谱 / z.ai）额度：优先 Coding Plan 配额，其次标准 API 余额。
use super::{Account, Balance, Snapshot, Window};
use serde_json::Value;
use std::io::Read;

const SERVICE: &str = "Daozai.GLM";

fn entry(slot: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, slot).map_err(|_| "无法打开 Windows 凭据管理器".into())
}
pub fn save_key(slot: &str, key: &str) -> Result<(), String> {
    entry(slot)?.set_password(key).map_err(|_| "无法安全保存 GLM API Key".into())
}
fn load_key(slot: &str) -> Result<String, String> {
    entry(slot)?.get_password().map_err(|_| "未找到 GLM API Key，请重新连接".into())
}
pub fn forget(account: &Account) {
    if let Some(slot) = &account.slot {
        if let Ok(entry) = entry(slot) { let _ = entry.delete_credential(); }
    }
}
pub fn slot_of(key: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, key.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in &digest.as_ref()[..8] { out.push_str(&format!("{byte:02x}")); }
    out
}

fn get(agent: &ureq::Agent, url: &str, auth: &str) -> Result<Value, ureq::Error> {
    let response = agent.get(url)
        .set("Authorization", auth)
        .set("Accept", "application/json")
        .set("User-Agent", "Daozai")
        .call()?;
    let mut body = String::new();
    let _ = response.into_reader().take(512 * 1024).read_to_string(&mut body);
    Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
}

fn auth_error(v: &Value) -> bool {
    matches!(v["code"].as_i64(), Some(401 | 403 | 1001)) || matches!(v["msg"].as_str(), Some(m) if m.contains("认证") || m.contains("credential"))
}

pub fn fetch(agent: &ureq::Agent, account: &Account) -> Result<Snapshot, (String, u64)> {
    let slot = account.slot.as_deref().ok_or(("GLM 账号缺少安全存储标识".to_string(), 0))?;
    let key = load_key(slot).map_err(|e| (e, 0))?;
    let bearer = format!("Bearer {key}");

    // 1) z.ai Coding Plan quota
    let quota_url = "https://api.z.ai/api/monitor/usage/quota/limit";
    let mut quota_error = None;
    match get(agent, quota_url, &bearer) {
        Ok(value) if !auth_error(&value) => {
            if let Ok(snapshot) = parse(&value) { return Ok(snapshot); }
            quota_error = Some("Coding Plan 未返回可用额度".to_string());
        }
        Ok(_) => quota_error = Some("GLM API Key 无效或已失效".to_string()),
        Err(ureq::Error::Status(401 | 403, _)) => quota_error = Some("GLM API Key 无效或已失效".to_string()),
        Err(ureq::Error::Status(code, _)) if code == 404 => {}
        Err(_) => {}
    }

    // 2) 中国站 open.bigmodel.cn（监控接口用原始 Key）
    let china = "https://open.bigmodel.cn/api/monitor/usage/quota/limit";
    if let Ok(value) = get(agent, china, &key) {
        if !auth_error(&value) {
            if let Ok(snapshot) = parse(&value) { return Ok(snapshot); }
        }
    }

    // 3) 标准 API 余额：z.ai / open.bigmodel.cn
    for (host, auth) in [("https://api.z.ai", bearer.as_str()), ("https://open.bigmodel.cn", bearer.as_str())] {
        let url = format!("{host}/api/biz/account/query-customer-account-report");
        match get(agent, &url, auth) {
            Ok(value) => {
                if let Some(data) = value.get("data") {
                    if data.get("balance").is_some() || data.get("availableBalance").is_some() {
                        return parse_balance(data).map_err(|e| (e, 60));
                    }
                }
            }
            Err(ureq::Error::Status(401 | 403, _)) => {}
            Err(_) => {}
        }
    }
    Err((quota_error.unwrap_or_else(|| "无法连接 GLM，请检查 API Key、网络或代理".into()), 60))
}

fn percentage(limit: &Value) -> Option<f32> {
    limit["percentage"].as_f64().or_else(|| limit["percentage"].as_str().and_then(|s| s.parse().ok())).map(|n| n.clamp(0.0, 100.0) as f32)
}
fn reset_at(limit: &Value) -> Option<i64> {
    limit["nextResetTime"].as_i64().map(|ms| ms / 1000)
}
fn parse_quota(limits: &[Value]) -> Result<Snapshot, String> {
    let typed: Vec<&Value> = limits.iter().filter(|v| matches!(v["type"].as_str(), Some("TOKENS_LIMIT" | "CREDIT_LIMIT"))).collect();
    let source: Vec<&Value> = if typed.iter().any(|v| v["type"] == "TOKENS_LIMIT") {
        typed.into_iter().filter(|v| v["type"] == "TOKENS_LIMIT").collect()
    } else { typed };
    let mut windows = Vec::new();
    for limit in source {
        let Some(used) = percentage(limit) else { continue };
        let unit = limit["unit"].as_i64().unwrap_or(0);
        let label = match unit { 3 => "5 小时", 6 => "本周", 7 => "本月", _ => "额度" };
        if windows.iter().any(|w: &Window| w.label == label) { continue; }
        windows.push(Window { label: label.into(), seconds: None, remaining: 100.0 - used, reset: reset_at(limit) });
    }
    if windows.is_empty() { return Err("Coding Plan 没有可用额度".into()); }
    Ok(Snapshot { plan: "Coding Plan".into(), windows, extra: vec![], at: chrono::Utc::now().timestamp(), balance: None })
}

fn parse_balance(data: &Value) -> Result<Snapshot, String> {
    let num = |key: &str| data[key].as_f64().or_else(|| data[key].as_str().and_then(|s| s.parse().ok())).filter(|n| n.is_finite());
    let total = num("availableBalance").or_else(|| num("balance")).ok_or("GLM 没有返回余额信息")?;
    Ok(Snapshot {
        plan: String::new(),
        windows: vec![],
        extra: vec![],
        balance: Some(Balance { currency: "CNY".into(), total, granted: num("giveAmount").unwrap_or(0.0), topped_up: num("rechargeAmount").unwrap_or(0.0) }),
        at: chrono::Utc::now().timestamp(),
    })
}

pub fn parse(v: &Value) -> Result<Snapshot, String> {
    if let Some(limits) = v.pointer("/data/limits").and_then(|v| v.as_array()) {
        return parse_quota(limits);
    }
    if let Some(data) = v.get("data") {
        if data.get("balance").is_some() || data.get("availableBalance").is_some() {
            return parse_balance(data);
        }
    }
    Err("GLM 没有返回有效的额度数据".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coding_plan_keeps_session_and_weekly_windows() {
        let snapshot = parse(&serde_json::json!({"code":200,"data":{"limits":[
            {"type":"TOKENS_LIMIT","percentage":10,"unit":3,"nextResetTime":1738368000000i64},
            {"type":"TOKENS_LIMIT","percentage":25,"unit":6,"nextResetTime":1788875329000i64}
        ]}})).unwrap();
        assert_eq!(snapshot.plan, "Coding Plan");
        assert_eq!(snapshot.rings().len(), 2);
        assert!((snapshot.rings()[0].remaining - 90.0).abs() < 0.01);
        assert!((snapshot.rings()[1].remaining - 75.0).abs() < 0.01);
    }

    #[test]
    fn credit_limits_and_standard_balance_are_supported() {
        let credit = parse(&serde_json::json!({"data":{"limits":[{"type":"CREDIT_LIMIT","percentage":18,"unit":3}]}})).unwrap();
        assert!((credit.rings()[0].remaining - 82.0).abs() < 0.01);
        let balance = parse(&serde_json::json!({"success":true,"data":{"availableBalance":98.21,"giveAmount":10.0,"rechargeAmount":88.21}})).unwrap();
        assert!((balance.balance.unwrap().total - 98.21).abs() < 0.01);
    }

    #[test]
    fn slot_is_stable_and_hides_key() {
        let key = "synthetic-glm-key-123456";
        assert_eq!(slot_of(key), slot_of(key));
        assert_eq!(slot_of(key).len(), 16);
        assert!(!slot_of(key).contains("synthetic"));
    }
}

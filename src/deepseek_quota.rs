//! DeepSeek 账户余额：官方只提供只读的 /user/balance，没有周期额度。
use super::{Account, Balance, Snapshot};
use serde_json::Value;
use std::io::Read;

const SERVICE: &str = "Daozai.DeepSeek";
const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

fn entry(slot: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, slot).map_err(|_| "无法打开 Windows 凭据管理器".into())
}

pub fn save_key(slot: &str, key: &str) -> Result<(), String> {
    entry(slot)?
        .set_password(key)
        .map_err(|_| "无法安全保存 DeepSeek API Key".into())
}

#[cfg(test)]
pub(super) fn key_exists(slot: &str) -> bool {
    entry(slot).ok().and_then(|entry| entry.get_password().ok()).is_some()
}

fn load_key(slot: &str) -> Result<String, String> {
    entry(slot)?
        .get_password()
        .map_err(|_| "未找到 DeepSeek API Key，请重新连接".into())
}

pub fn forget(account: &Account) {
    if let Some(slot) = &account.slot {
        if let Ok(entry) = entry(slot) {
            let _ = entry.delete_credential();
        }
    }
}

/// API Key 不写进配置文件；用摘要做凭据管理器里的稳定标识。
pub fn slot_of(key: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, key.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in &digest.as_ref()[..8] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn fetch(agent: &ureq::Agent, account: &Account) -> Result<Snapshot, (String, u64)> {
    let slot = account.slot.as_deref().ok_or(("DeepSeek 账号缺少安全存储标识".to_string(), 0))?;
    let key = load_key(slot).map_err(|e| (e, 0))?;
    let response = agent
        .get(BALANCE_URL)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Accept", "application/json")
        .set("User-Agent", "Daozai")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(401 | 403, _) => ("DeepSeek API Key 无效或已失效，请重新连接".into(), 0),
            ureq::Error::Status(429, r) => ("请求过于频繁，稍后自动重试".into(), r.header("Retry-After").and_then(|s| s.parse().ok()).unwrap_or(180).clamp(30, 3600)),
            ureq::Error::Status(n, _) => (format!("DeepSeek 余额服务暂不可用（{n}）"), 60),
            _ => ("无法连接 DeepSeek，请检查网络或代理".into(), 30),
        })?;
    let mut body = String::new();
    response
        .into_reader()
        .take(256 * 1024)
        .read_to_string(&mut body)
        .map_err(|_| ("读取 DeepSeek 余额响应失败".into(), 30))?;
    let value: Value = serde_json::from_str(&body)
        .map_err(|_| ("DeepSeek 没有返回有效的余额数据".into(), 60))?;
    parse(&value).map_err(|e| (e, 60))
}

fn money(info: &Value, key: &str) -> Option<f64> {
    info[key]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| info[key].as_f64())
        .filter(|n| n.is_finite())
}

pub fn parse(v: &Value) -> Result<Snapshot, String> {
    let infos = v["balance_infos"]
        .as_array()
        .filter(|list| !list.is_empty())
        .ok_or("DeepSeek 没有返回余额信息")?;
    let info = infos
        .iter()
        .find(|v| v["currency"].as_str() == Some("CNY"))
        .or_else(|| infos.first())
        .unwrap();
    let total = money(info, "total_balance").ok_or("DeepSeek 余额字段不完整")?;
    let available = v["is_available"].as_bool().unwrap_or(true);
    Ok(Snapshot {
        plan: if available { String::new() } else { "不可用".into() },
        windows: vec![],
        extra: vec![],
        balance: Some(Balance {
            currency: info["currency"].as_str().unwrap_or("CNY").into(),
            total,
            granted: money(info, "granted_balance").unwrap_or(0.0),
            topped_up: money(info, "topped_up_balance").unwrap_or(0.0),
        }),
        at: chrono::Utc::now().timestamp(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_uses_cny_and_keeps_breakdown() {
        let snapshot = parse(&serde_json::json!({
            "is_available": true,
            "balance_infos": [{"currency":"USD","total_balance":"1.00","granted_balance":"0.00","topped_up_balance":"1.00"},
                              {"currency":"CNY","total_balance":"110.00","granted_balance":"10.00","topped_up_balance":"100.00"}]
        })).unwrap();
        let balance = snapshot.balance.unwrap();
        assert_eq!(balance.currency, "CNY");
        assert!((balance.total - 110.0).abs() < 0.001);
        assert!((balance.granted - 10.0).abs() < 0.001);
        assert!((balance.topped_up - 100.0).abs() < 0.001);
        assert!(snapshot.windows.is_empty());
    }

    #[test]
    fn missing_or_invalid_balance_is_reported() {
        assert!(parse(&serde_json::json!({})).is_err());
        assert!(parse(&serde_json::json!({"balance_infos":[]})).is_err());
        assert!(parse(&serde_json::json!({"balance_infos":[{"currency":"CNY"}]})).is_err());
    }

    #[test]
    #[ignore = "读取本机已保存的 DeepSeek 账号余额"]
    fn live_local_deepseek_balance(){
        let config=crate::quota::Config::load();
        let account=config.accounts.iter().find(|a|a.provider==crate::quota::Provider::DeepSeek).expect("本机还没有 DeepSeek 账号");
        let agent=crate::quota::http_agent();
        match fetch(&agent,account){
            Ok(snapshot)=>println!("balance={:?}",snapshot.balance),
            Err(error)=>panic!("DeepSeek fetch failed: {error:?}"),
        }
    }

    #[test]
    fn slot_is_stable_and_does_not_contain_the_key() {
        let key = "sk-synthetic-deepseek-key-123456";
        assert_eq!(slot_of(key), slot_of(key));
        assert_ne!(slot_of(key), slot_of("sk-another"));
        assert_eq!(slot_of(key).len(), 16);
        assert!(!slot_of(key).contains("sk-"));
    }
}

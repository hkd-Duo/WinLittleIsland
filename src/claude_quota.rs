//! Claude 桌面版登录引用。所有原客户端文件仅通过只读接口访问。
use super::{native_auth,Account,Credential};
use ring::digest;
use serde_json::Value;
use std::{collections::BTreeMap,fs,path::{Path,PathBuf}};

const LOGIN_ERROR:&str="无法读取 Claude 桌面版登录，请在 Claude 中确认登录后重新识别";
pub(super) struct Profile {pub slot:String,pub label:String}
struct Login {slot:String,token:String,expires:i64,profile_only:bool,current:bool}

pub(super) fn paths(roaming:&Path,local:&Path)->Vec<PathBuf>{
    let mut paths=vec![roaming.join("Claude/config.json"),local.join("Claude/config.json")];
    if let Ok(entries)=fs::read_dir(local.join("Claude-Data")){
        for entry in entries.flatten(){if entry.file_name().to_string_lossy().to_ascii_lowercase().starts_with("claude_"){paths.push(entry.path().join("config.json"));}}
    }
    if let Ok(entries)=fs::read_dir(local.join("Packages")){
        for entry in entries.flatten(){if entry.file_name().to_string_lossy().to_ascii_lowercase().starts_with("claude_"){paths.push(entry.path().join("LocalCache/Roaming/Claude/config.json"));}}
    }
    paths.retain(|p|p.is_file());paths.sort();paths.dedup();paths
}
pub(super) fn is_desktop(path:&Path)->bool{
    super::login_json(path).is_ok_and(|v|v.get("oauth:tokenCacheV2").is_some()||v.get("oauth:tokenCache").is_some())
}
fn decode(config:&Value,key:&[u8],now:i64)->Result<Vec<Login>,String>{
    // 新缓存存在时不回退旧缓存，避免退出登录或切换账号后复活旧会话。
    let encrypted=config.get("oauth:tokenCacheV2").or_else(||config.get("oauth:tokenCache")).and_then(Value::as_str).ok_or(LOGIN_ERROR)?;
    let plain=native_auth::decrypt(key,encrypted).map_err(|_|LOGIN_ERROR)?;
    let entries:Value=serde_json::from_str(&plain).map_err(|_|LOGIN_ERROR)?;
    let mut accounts:BTreeMap<String,Login>=BTreeMap::new();
    for (cache_key,value) in entries.as_object().ok_or(LOGIN_ERROR)?{
        let Some((owner,tail))=cache_key.strip_prefix("acct:").and_then(|s|s.split_once('|')) else{continue};
        let Some((_,tail))=tail.split_once(':') else{continue};
        let Some((org,tail))=tail.split_once(':') else{continue};
        let Some(scopes)=tail.strip_prefix("https://api.anthropic.com:") else{continue};
        if owner.is_empty()||org.is_empty()||!scopes.split_whitespace().any(|s|s=="user:profile"){continue;}
        let Some(token)=value["token"].as_str().filter(|s|!s.trim().is_empty()) else{continue};
        let slot=digest::digest(&digest::SHA256,format!("{owner}|{org}").as_bytes()).as_ref().iter().map(|b|format!("{b:02x}")).collect::<String>();
        let login=Login{slot:slot.clone(),token:token.into(),expires:value["expiresAt"].as_i64().unwrap_or(0),profile_only:scopes=="user:profile",current:config["lastKnownAccountUuid"].as_str()==Some(owner)};
        let rank=|login:&Login|(login.expires>now,login.profile_only,login.expires);
        if accounts.get(&slot).is_none_or(|old|rank(&login)>rank(old)){accounts.insert(slot,login);}
    }
    let mut logins=accounts.into_values().collect::<Vec<_>>();logins.sort_by_key(|l|!l.current);Ok(logins)
}
fn logins(path:&Path)->Result<Vec<Login>,String>{
    let config=super::login_json(path)?;
    let key=native_auth::key(path).map_err(|_|LOGIN_ERROR)?;
    decode(&config,&key,chrono::Utc::now().timestamp_millis())
}
pub(super) fn profiles(path:&Path)->Result<Vec<Profile>,String>{
    let logins=logins(path)?;if logins.is_empty(){return Err("Claude 桌面版当前没有可读取的登录账号".into());}
    Ok(logins.into_iter().enumerate().map(|(i,l)|Profile{slot:l.slot,label:format!("Claude · 桌面版 {}",i+1)}).collect())
}
pub(super) fn credentials(account:&Account)->Result<Credential,String>{
    let stamp=fs::metadata(&account.source).and_then(|m|m.modified()).ok();
    let login=logins(&account.source)?.into_iter().find(|l|Some(l.slot.as_str())==account.slot.as_deref()).ok_or("Claude 桌面版账号已变化，请重新识别账号")?;
    if login.expires>0&&login.expires<=chrono::Utc::now().timestamp_millis(){return Err("Claude 桌面版登录已过期，请在 Claude 中确认登录后刷新".into());}
    Ok(Credential{token:login.token,account_id:None,identity:Some(login.slot),stamp})
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD,Engine};
    use ring::aead;
    use serde_json::json;
    fn sealed(value:&Value)->String{
        let key=aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_256_GCM,&[7;32]).unwrap());
        let mut bytes=serde_json::to_vec(value).unwrap();key.seal_in_place_append_tag(aead::Nonce::assume_unique_for_key([0;12]),aead::Aad::empty(),&mut bytes).unwrap();
        let mut blob=b"v10".to_vec();blob.extend_from_slice(&[0;12]);blob.extend(bytes);STANDARD.encode(blob)
    }
    #[test]
    fn desktop_scopes_share_one_account_and_profile_access_is_preferred(){
        let data=json!({
            "acct:personal|client:org:https://api.anthropic.com:user:inference user:profile":{"token":"synthetic-full","expiresAt":3000},
            "acct:personal|client:org:https://api.anthropic.com:user:profile":{"token":"synthetic-profile","expiresAt":2000},
            "acct:work|client:other-org:https://api.anthropic.com:user:profile":{"token":"synthetic-work","expiresAt":2000}
        });
        let config=json!({"oauth:tokenCacheV2":sealed(&data),"lastKnownAccountUuid":"work"});
        let logins=decode(&config,&[7;32],1000).unwrap();assert_eq!(logins.len(),2);assert_eq!(logins[0].token,"synthetic-work");assert_eq!(logins[1].token,"synthetic-profile");
        let mut changed=data.clone();changed["acct:personal|client:org:https://api.anthropic.com:user:profile"]["token"]=json!("synthetic-renewed");
        let updated=decode(&json!({"oauth:tokenCacheV2":sealed(&changed),"lastKnownAccountUuid":"work"}),&[7;32],1000).unwrap();
        assert_eq!(logins[1].slot,updated[1].slot,"续期不能创建新账号");
    }
    #[test]
    fn desktop_logout_does_not_fall_back_to_stale_tokens_or_other_accounts(){
        let old=sealed(&json!({"acct:old|client:org:https://api.anthropic.com:user:profile":{"token":"synthetic-old","expiresAt":2000}}));
        assert_eq!(decode(&json!({"oauth:tokenCacheV2":sealed(&json!({})),"oauth:tokenCache":old}),&[7;32],1000).unwrap().len(),0);
        assert!(decode(&json!({"oauth:tokenCacheV2":"invalid","oauth:tokenCache":old}),&[7;32],1000).is_err());
        assert_eq!(decode(&json!({"oauth:tokenCache":old}),&[7;32],1000).unwrap().len(),1);
    }
    #[test]
    fn expired_profile_scope_does_not_hide_a_valid_login(){
        let config=json!({"oauth:tokenCacheV2":sealed(&json!({
            "acct:personal|client:org:https://api.anthropic.com:user:profile":{"token":"synthetic-expired","expiresAt":500},
            "acct:personal|client:org:https://api.anthropic.com:user:profile user:inference":{"token":"synthetic-valid","expiresAt":2000}
        }))});
        assert_eq!(decode(&config,&[7;32],1000).unwrap()[0].token,"synthetic-valid");
        assert!(decode(&config,&[8;32],1000).is_err(),"错误的本机密钥不能解密登录");
    }
    #[cfg(windows)]
    #[test]
    fn desktop_file_discovery_is_read_only_and_binds_the_selected_account(){
        use windows::Win32::{Foundation::{HLOCAL,LocalFree},Security::Cryptography::{CryptProtectData,CRYPT_INTEGER_BLOB,CRYPTPROTECT_UI_FORBIDDEN}};
        let dir=std::env::temp_dir().join(format!("daozai-claude-{}",std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let data=dir.join("Claude-Data/claude_example");fs::create_dir_all(&data).unwrap();let path=data.join("config.json");
        let key=[7u8;32];let input=CRYPT_INTEGER_BLOB{cbData:32,pbData:key.as_ptr().cast_mut()};let mut output=CRYPT_INTEGER_BLOB::default();
        unsafe{CryptProtectData(&input,None,None,None,None,CRYPTPROTECT_UI_FORBIDDEN,&mut output).unwrap();}
        let mut protected=b"DPAPI".to_vec();protected.extend_from_slice(unsafe{std::slice::from_raw_parts(output.pbData,output.cbData as usize)});unsafe{let _=LocalFree(HLOCAL(output.pbData.cast()));}
        let state=json!({"os_crypt":{"encrypted_key":STANDARD.encode(protected)}}).to_string();fs::write(data.join("Local State"),&state).unwrap();
        let source=json!({"oauth:tokenCacheV2":sealed(&json!({"acct:personal|client:org:https://api.anthropic.com:user:profile":{"token":"synthetic-personal","expiresAt":4102444800000i64}}))}).to_string();fs::write(&path,&source).unwrap();
        assert_eq!(paths(&dir.join("roaming"),&dir),vec![path.clone()]);
        let mut config=super::super::Config::default();assert_eq!(config.import_claude(&path,"").unwrap(),1);
        config.accounts[0].alias="保留桌面版备注".into();config.accounts[0].enabled=false;let original=config.clone();
        assert_eq!(config.import_claude(&path,"").unwrap(),0);assert_eq!(config,original);assert_eq!(credentials(&config.accounts[0]).unwrap().token,"synthetic-personal");
        assert_eq!(fs::read_to_string(&path).unwrap(),source);assert_eq!(fs::read_to_string(data.join("Local State")).unwrap(),state);
        let exported=config.json().to_string();assert!(!exported.contains("synthetic-personal")&&!exported.contains("refreshToken"),"岛仔配置不能复制原客户端令牌");
        fs::write(&path,json!({"oauth:tokenCacheV2":sealed(&json!({"acct:other|client:org:https://api.anthropic.com:user:profile":{"token":"synthetic-other","expiresAt":4102444800000i64}}))}).to_string()).unwrap();
        assert!(credentials(&config.accounts[0]).is_err(),"客户端切换账号后不能把另一账号额度显示到旧账号上");
        let _=fs::remove_dir_all(dir);
    }
}

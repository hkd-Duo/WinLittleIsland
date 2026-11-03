#![cfg(windows)]

use daozai::calendar::{self, Config, Provider};

#[test]
fn failed_configuration_preserves_old_credentials() {
    let id = format!("{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    let path = std::env::temp_dir().join(format!("daozai-credential-test-{id}"));
    std::fs::write(&path, "不是目录").unwrap();
    std::env::set_var("DAOZAI_DATA_DIR", &path);
    let old = Config { provider: Provider::Feishu, username: format!("test-old-{id}"), href: None, server: None };
    let new = Config { username: format!("test-new-{id}"), ..old.clone() };
    calendar::secrets::save(&old, "旧密码").unwrap();
    assert!(calendar::save_connection(&new, "新密码", Some(&old)).is_err());
    assert_eq!(calendar::secrets::load(&old).unwrap(), "旧密码");
    assert!(calendar::secrets::load(&new).is_err());
    assert!(calendar::save_connection(&old, "更新后的密码", Some(&old)).is_err());
    assert_eq!(calendar::secrets::load(&old).unwrap(), "旧密码");
    calendar::secrets::delete(&old);
    calendar::secrets::delete(&new);
    std::fs::remove_file(path).unwrap();
}

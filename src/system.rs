//! Windows 状态采样与经用户授权的通知读取。
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver};
use windows::core::{Result, HSTRING, PCWSTR, PWSTR};
use windows::Foundation::Size;
use windows::Storage::Streams::DataReader;
use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, GetLastInputInfo, LASTINPUTINFO,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::UI::Notifications::Management::{
    UserNotificationListener, UserNotificationListenerAccessStatus,
};
use windows::UI::Notifications::{NotificationBinding, NotificationKinds, NotificationVisual, UserNotification};

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub volume: Option<(u32, bool)>,
    pub audio_endpoint: Option<String>,
    pub caps: bool,
    pub network: bool,
    pub network_kind: NetworkKind,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Observation {
    pub system: Snapshot,
    pub input: Option<crate::input::State>,
    pub fullscreen: Option<crate::display_layout::Bounds>,
}

/// 跨进程输入法查询和 COM 状态采样留在后台，只在状态改变时唤醒界面。
pub fn observe(ctx:eframe::egui::Context,initial:Snapshot,input:Option<crate::input::State>)->Receiver<Observation> {
    let(tx,rx)=mpsc::sync_channel(1);
    std::thread::Builder::new().name("daozai-system".into()).spawn(move||{
        let mut current=Observation{system:initial,input,fullscreen:None};
        let mut delivered=None;
        let mut sampled=crate::poll_clock::PollClock::default();
        let mut input_at=sampled;
        let mut identity_at=sampled;
        loop {
            // 输入法身份查询是跨进程 COM 调用，500ms 采样足够；按键切换由 switch_recent 兜底。
            if identity_at.ready(std::time::Duration::from_millis(500)){
                let identity=crate::input::State::identity();
                let changed=identity.map(|state|(state.context,state.layout))!=current.input.map(|state|(state.context,state.layout));
                if changed || crate::input::switch_recent() || input_at.ready(std::time::Duration::from_secs(1)){
                    current.input=crate::input::State::read();input_at=crate::poll_clock::PollClock::now();
                }
                identity_at=crate::poll_clock::PollClock::now();
            }
            if sampled.ready(std::time::Duration::from_millis(1000)){
                current.system=Snapshot::read();current.fullscreen=fullscreen();sampled=crate::poll_clock::PollClock::now();
            }
            if delivered.as_ref()!=Some(&current) {
                match tx.try_send(current.clone()) {
                    Ok(())=>{delivered=Some(current.clone());ctx.request_repaint();},
                    Err(mpsc::TrySendError::Disconnected(_))=>return,
                    Err(mpsc::TrySendError::Full(_))=>{},
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }).ok();
    rx
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetworkKind { Wifi, Ethernet, #[default] Other }
impl Snapshot {
    pub fn read() -> Self {
        unsafe {
            let device=audio_device().ok();
            let audio_endpoint=device.as_ref().and_then(|d|d.GetId().ok()).and_then(|id|{
                let text=id.to_string().ok();
                windows::Win32::System::Com::CoTaskMemFree(Some(id.0 as _));
                text
            });
            let volume = device.and_then(|d|d.Activate::<IAudioEndpointVolume>(CLSCTX_ALL,None).ok()).and_then(|a| {
                Some((
                    (a.GetMasterVolumeLevelScalar().ok()? * 100.0).round() as u32,
                    a.GetMute().ok()?.as_bool(),
                ))
            });
            let profile=windows::Networking::Connectivity::NetworkInformation::GetInternetConnectionProfile().ok();
            let network=profile.as_ref().and_then(|p|p.GetNetworkConnectivityLevel().ok()).is_some_and(|v|v==windows::Networking::Connectivity::NetworkConnectivityLevel::InternetAccess);
            let network_kind=profile.as_ref().map(|p| {
                if p.IsWlanConnectionProfile().unwrap_or(false){NetworkKind::Wifi}
                else if p.NetworkAdapter().ok().and_then(|a|a.IanaInterfaceType().ok())==Some(6){NetworkKind::Ethernet}
                else{NetworkKind::Other}
            }).unwrap_or_default();
            Self{volume,audio_endpoint,network,network_kind,caps:GetKeyState(0x14)&1!=0}
        }
    }
    pub fn changes(&self, old: &Self) -> Vec<(String, String)> {
        let mut out = vec![];
        if self.audio_endpoint == old.audio_endpoint && self.volume != old.volume {
            if let Some((v, mute)) = self.volume {
                out.push((
                    "系统音量".into(),
                    if mute {
                        "已静音".into()
                    } else {
                        format!("音量 {v}%")
                    },
                ));
            }
        }
        if self.caps != old.caps {
            out.push((
                "大写锁定".into(),
                if self.caps { "已开启" } else { "已关闭" }.into(),
            ));
        }
        if self.network != old.network || self.network && self.network_kind != old.network_kind {
            let kind=if self.network {self.network_kind}else{old.network_kind};
            let status=if self.network {"网络已连接"}else{"网络已断开"};
            out.push((
                "网络连接".into(),
                match kind {NetworkKind::Wifi=>format!("Wi-Fi · {status}"),NetworkKind::Ethernet=>format!("以太网 · {status}"),NetworkKind::Other=>status.into()},
            ));
        }
        out
    }
}
unsafe fn audio_device() -> Result<IMMDevice> {    thread_local! {static INITIALIZED:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};}
    INITIALIZED.with(|v| {
        if !v.replace(true) {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
    });
    // 每个采样线程复用枚举器，但每次仍查询当前默认设备，保留热插拔响应。
    thread_local! {static ENUMERATOR:std::cell::RefCell<Option<IMMDeviceEnumerator>>=const{std::cell::RefCell::new(None)};}
    ENUMERATOR.with(|slot| {
        let mut slot=slot.borrow_mut();
        if slot.is_none(){*slot=Some(CoCreateInstance(&MMDeviceEnumerator,None,CLSCTX_ALL)?);}
        let result=slot.as_ref().unwrap().GetDefaultAudioEndpoint(eRender,eConsole);
        if result.is_err(){*slot=None;}
        result
    })
}
unsafe fn audio() -> Result<IAudioEndpointVolume> {audio_device()?.Activate(CLSCTX_ALL,None)}
/// 设置主音量（0..100）；同时解除静音，拖动音量条时立刻能听到。
#[cfg(windows)]
pub fn set_volume(percent: u32) {
    unsafe {
        if let Ok(a) = audio() {
            let _ = a.SetMasterVolumeLevelScalar((percent.min(100) as f32) / 100.0, std::ptr::null());
            let _ = a.SetMute(false, std::ptr::null());
        }
    }
}

/// 切换静音，返回切换后的静音状态（拿不到设备时返回 None）。
#[cfg(windows)]
pub fn toggle_mute() -> Option<bool> {
    unsafe {
        let a = audio().ok()?;
        let muted = a.GetMute().ok()?.as_bool();
        a.SetMute(!muted, std::ptr::null()).ok()?;
        Some(!muted)
    }
}

pub fn idle_seconds() -> f64 {
    unsafe {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            ..Default::default()
        };
        if GetLastInputInfo(&mut info).as_bool() {
            windows::Win32::System::SystemInformation::GetTickCount().wrapping_sub(info.dwTime)
                as f64
                / 1000.0
        } else {
            0.0
        }
    }
}
pub fn fullscreen() -> Option<crate::display_layout::Bounds> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == windows::Win32::System::Threading::GetCurrentProcessId() {
            return None;
        }
        // 桌面与资源管理器桌面窗口不能当成全屏游戏。
        let mut cls = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut cls);
        let cls = String::from_utf16_lossy(&cls[..n.max(0) as usize]);
        if matches!(cls.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
            return None;
        }
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_err() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info).as_bool()
        {
            return None;
        }
        let covers=r.left <= info.rcMonitor.left
            && r.top <= info.rcMonitor.top
            && r.right >= info.rcMonitor.right
            && r.bottom >= info.rcMonitor.bottom;
        covers.then_some(crate::display_layout::Bounds {left:info.rcMonitor.left,top:info.rcMonitor.top,right:info.rcMonitor.right,bottom:info.rcMonitor.bottom})
    }
}
pub(crate) fn is_browser_process(name:&str)->bool {
    ["chrome.exe","msedge.exe","firefox.exe","brave.exe","vivaldi.exe","opera.exe",
        "arc.exe","zen.exe","chromium.exe","librewolf.exe","floorp.exe","waterfox.exe",
        "duckduckgo.exe","360se.exe","360chrome.exe","qqbrowser.exe","sogouexplorer.exe","quark.exe"]
        .iter().any(|browser|name.eq_ignore_ascii_case(browser))
}

/// 岛在透明画布左侧，画布宽为 CANVAS_W 逻辑点；窗口物理宽度随岛体大小一起缩放，
/// 所以 ppp（含岛体缩放）= 窗口宽度 / CANVAS_W，岛中心固定在窗口左侧 WIN_W/2 处。
fn browser_overlaps_island(browser:RECT,island:RECT)->bool {
    let scale=(island.right-island.left) as f32/crate::app::CANVAS_W;
    let center=island.left as f32+crate::app::WIN_W*scale/2.0;
    let half=crate::app::PW*scale/2.0;
    (browser.left as f32)<center+half && (browser.right as f32)>center-half
        && (browser.top as f32)<island.top as f32+crate::app::BAR_H*scale && browser.bottom>island.top
}

pub fn browser_at_top(island:HWND,settings:&crate::model::Settings)->Option<bool> {
    unsafe {
        let foreground=GetForegroundWindow();
        if foreground.0.is_null(){return None;}
        let mut pid=0;
        GetWindowThreadProcessId(foreground,Some(&mut pid));
        // 设置、托盘和岛内输入沿用此前的避让状态，避免自身窗口引起反复切换。
        if pid==0 || pid==GetCurrentProcessId(){return None;}
        let mut browser_rect=RECT::default();let mut island_rect=RECT::default();
        if GetWindowRect(foreground,&mut browser_rect).is_err() || GetWindowRect(island,&mut island_rect).is_err(){return None;}
        if !browser_overlaps_island(browser_rect,island_rect){return Some(false);}
        let Ok(process)=OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,pid) else{return Some(false)};
        let mut path=[0u16;1024];let mut length=path.len() as u32;
        let result=QueryFullProcessImageNameW(process,PROCESS_NAME_WIN32,PWSTR(path.as_mut_ptr()),&mut length);
        let _=CloseHandle(process);
        if result.is_err(){return Some(false);}
        let path=String::from_utf16_lossy(&path[..length as usize]);
        Some(avoidance_matches(&path,settings))
    }
}

pub enum Incoming {
    Revoked,
    Status(String),
    Message(String, String, String, Option<(u32, u32, Vec<u8>)>),
}
/// 通知来源常以快捷方式文件名出现（例如 "PowerShell 7 (x64).lnk"），去掉后缀并兜底。
fn clean_app_name(display: &str) -> String {
    let trimmed = display.trim();
    let trimmed = trimmed.strip_suffix(".lnk").unwrap_or(trimmed).trim();
    if trimmed.is_empty() {
        "Windows 通知".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 新式 Toast 用 ToastGeneric；旧式应用仍可能使用 ToastText/ToastImageAndText 模板。
fn notification_binding(visual: &NotificationVisual) -> Option<NotificationBinding> {
    const CANDIDATES: &[&str] = &[
        "ToastGeneric",
        "ToastText01",
        "ToastText02",
        "ToastText03",
        "ToastText04",
        "ToastImageAndText01",
        "ToastImageAndText02",
        "ToastImageAndText03",
        "ToastImageAndText04",
    ];
    CANDIDATES
        .iter()
        .find_map(|name| visual.GetBinding(&HSTRING::from(*name)).ok())
}

fn notification_text(n: &UserNotification) -> Option<String> {
    let binding = notification_binding(&n.Notification().ok()?.Visual().ok()?)?;
    let texts = binding.GetTextElements().ok()?;
    let mut parts = vec![];
    for i in 0..texts.Size().ok()? {
        let text = texts.GetAt(i).ok()?.Text().ok()?.to_string();
        if !text.trim().is_empty() {
            parts.push(text);
        }
    }
    Some(parts.join(" · "))
}

/// 优先读取封装应用自带的 Logo；Win32 应用通常返回空，再用 Shell 的 AppsFolder 图标兜底。
fn appinfo_logo_rgba(n: &UserNotification) -> Option<(u32, u32, Vec<u8>)> {
    let info = n.AppInfo().ok()?.DisplayInfo().ok()?;
    let logo = info
        .GetLogo(Size {
            Width: 48.0,
            Height: 48.0,
        })
        .ok()?;
    let stream = logo.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()?;
    if size == 0 || size > 512 * 1024 {
        return None;
    }
    let input = stream.GetInputStreamAt(0).ok()?;
    let reader = DataReader::CreateDataReader(&input).ok()?;
    reader.LoadAsync(size as u32).ok()?.get().ok()?;
    let mut bytes = vec![0u8; size as usize];
    reader.ReadBytes(&mut bytes).ok()?;
    let image = image::load_from_memory(&bytes).ok()?.to_rgba8();
    Some((image.width(), image.height(), image.into_raw()))
}

/// 把 HICON 通过一个 32bpp DIB 转成直通 RGBA；没有 alpha 的旧图标再用 AND mask 还原透明度。
unsafe fn hicon_to_rgba(hicon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<(u32, u32, Vec<u8>)> {
    use std::mem::size_of;
    use windows::Win32::Graphics::Gdi::{
        BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CreateCompatibleDC, CreateDIBSection,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, GetObjectW, HGDIOBJ, SelectObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DI_NORMAL, DrawIconEx, GetIconInfo, ICONINFO};
    let mut info = ICONINFO::default();
    GetIconInfo(hicon, &mut info).ok()?;
    let result = (|| -> Option<(u32, u32, Vec<u8>)> {
        let mut bitmap = BITMAP::default();
        if GetObjectW(
            HGDIOBJ(info.hbmColor.0),
            size_of::<BITMAP>() as i32,
            Some(&mut bitmap as *mut _ as *mut core::ffi::c_void),
        ) == 0
        {
            return None;
        }
        let (w, h) = (bitmap.bmWidth, bitmap.bmHeight);
        if w <= 0 || h <= 0 || w > 256 || h > 256 {
            return None;
        }
        let hdc = CreateCompatibleDC(None);
        if hdc.0.is_null() {
            return None;
        }
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(hdc, &bi, DIB_RGB_COLORS, &mut bits, None, 0);
        let Ok(dib) = dib else {
            let _ = DeleteDC(hdc);
            return None;
        };
        let old = SelectObject(hdc, HGDIOBJ(dib.0));
        let drawn = DrawIconEx(hdc, 0, 0, hicon, w, h, 0, None, DI_NORMAL);
        SelectObject(hdc, old);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        if drawn.is_ok() && !bits.is_null() {
            let data = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
            let mut any_alpha = false;
            for px in data.chunks_exact(4) {
                if px[3] > 0 {
                    any_alpha = true;
                }
                rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
            if !any_alpha {
                let stride = (((w + 31) / 32) * 4) as usize;
                let mut mask = vec![0u8; stride * h as usize];
                let mut mi = BITMAPINFO::default();
                mi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
                mi.bmiHeader.biWidth = w;
                mi.bmiHeader.biHeight = -h;
                mi.bmiHeader.biPlanes = 1;
                mi.bmiHeader.biBitCount = 1;
                mi.bmiHeader.biCompression = BI_RGB.0;
                let hdc2 = CreateCompatibleDC(None);
                if !hdc2.0.is_null() {
                    if GetDIBits(hdc2, info.hbmMask, 0, h as u32, Some(mask.as_mut_ptr() as *mut core::ffi::c_void), &mut mi, DIB_RGB_COLORS) > 0 {
                        for y in 0..h as usize {
                            for x in 0..w as usize {
                                let transparent = (mask[y * stride + x / 8] >> (7 - (x % 8))) & 1 == 1;
                                rgba[(y * w as usize + x) * 4 + 3] = if transparent { 0 } else { 255 };
                            }
                        }
                    }
                    let _ = DeleteDC(hdc2);
                }
            }
        }
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(hdc);
        (rgba.len() == (w * h * 4) as usize).then_some((w as u32, h as u32, rgba))
    })();
    if !info.hbmColor.0.is_null() {
        let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
    }
    if !info.hbmMask.0.is_null() {
        let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
    }
    result
}

fn shell_app_icon_rgba(aumid: &str) -> Option<(u32, u32, Vec<u8>)> {
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHParseDisplayName, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_PIDL,
    };
    use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;
    unsafe {
        let path = format!("shell:AppsFolder\\{aumid}");
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None).ok()?;
        let mut file = SHFILEINFOW::default();
        let flags = SHGFI_PIDL | SHGFI_ICON | SHGFI_LARGEICON;
        let got = SHGetFileInfoW(
            PCWSTR(pidl as *const u16),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut file),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        );
        CoTaskMemFree(Some(pidl as *const core::ffi::c_void));
        if got == 0 || file.hIcon.0.is_null() {
            return None;
        }
        let icon = file.hIcon;
        let rgba = hicon_to_rgba(icon);
        let _ = DestroyIcon(icon);
        rgba
    }
}

/// 读取通知来源应用的小图标；读取失败不影响通知本身。
fn notification_icon(n: &UserNotification, aumid: &str) -> Option<(u32, u32, Vec<u8>)> {
    appinfo_logo_rgba(n).or_else(|| shell_app_icon_rgba(aumid))
}

/// 返回（应用名、AppUserModelId、正文）；无文字的通知也会返回空正文，交给界面显示通用提示。
fn notification_content(n: &UserNotification) -> (String, String, String) {
    let app = n
        .AppInfo()
        .and_then(|a| a.DisplayInfo())
        .and_then(|d| d.DisplayName())
        .map(|s| clean_app_name(&s.to_string()))
        .unwrap_or_else(|_| "Windows 通知".to_string());
    let id = n
        .AppInfo()
        .and_then(|a| a.AppUserModelId())
        .map(|s| s.to_string())
        .unwrap_or_else(|_| app.clone());
    let body = notification_text(n).unwrap_or_default();
    (app, id, body)
}
/// 必须由窗口线程发起权限请求，后台线程仅等待并读取新增通知。
pub fn connect() -> Result<Receiver<Incoming>> {
    let listener = UserNotificationListener::Current()?;
    let request = listener.RequestAccessAsync()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let run = || -> Result<()> {
            let _ = unsafe {
                windows::Win32::System::WinRT::RoInitialize(
                    windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
                )
            };
            if request.get()? != UserNotificationListenerAccessStatus::Allowed {
                let _ = tx.send(Incoming::Revoked);
                let _ = tx.send(Incoming::Status(
                    "Windows 未授予通知读取权限，可在系统设置中调整。".into(),
                ));
                return Ok(());
            }
            let listener = UserNotificationListener::Current()?;
            // 通知 Id 只在一个应用内唯一，必须带上 AppUserModelId；同时记录正文，
            // 这样同一会话/同一条通知更新内容时也能再次上岛。
            let mut seen: HashMap<(u32, String), (String, String)> = HashMap::new();
            let mut first = true;
            loop {
                if listener.GetAccessStatus()? != UserNotificationListenerAccessStatus::Allowed {
                    let _ = tx.send(Incoming::Revoked);
                    let _ = tx.send(Incoming::Status("通知权限已被关闭".into()));
                    return Ok(());
                }
                let items = listener
                    .GetNotificationsAsync(NotificationKinds::Toast)?
                    .get()?;
                let mut current: HashSet<(u32, String)> = HashSet::new();
                for i in 0..items.Size()? {
                    let n = items.GetAt(i)?;
                    let id = n.Id()?;
                    let (app, app_id, body) = notification_content(&n);
                    let key = (id, app_id.clone());
                    let value = (app.clone(), body.clone());
                    current.insert(key.clone());
                    if !first && seen.get(&key) != Some(&value) {
                        let icon = notification_icon(&n, &app_id);
                        if tx.send(Incoming::Message(app, app_id, body, icon)).is_err() {
                            return Ok(());
                        }
                    }
                    seen.insert(key, value);
                }
                if first {
                    let _ = tx.send(Incoming::Status(format!(
                        "Windows 已授权 · 当前可见 {} 条通知",
                        items.Size()?
                    )));
                    first = false;
                }
                seen.retain(|key, _| current.contains(key));
                std::thread::sleep(std::time::Duration::from_secs(2));
                if tx
                    .send(Incoming::Status(format!(
                        "正在监听 · 当前可见 {} 条通知",
                        seen.len()
                    )))
                    .is_err()
                {
                    return Ok(());
                }
            }
        };
        if let Err(e) = run() {
            let _ = tx.send(Incoming::Status(format!(
                "Windows 暂不允许读取通知：{}。此功能可能需要打包应用身份。",
                e.code()
            )));
        }
    });
    Ok(rx)
}

pub fn pick_files() -> Result<Vec<std::path::PathBuf>> {
    pick_files_for("添加文件到置物架",true)
}

pub fn pick_files_for(title:&str,multiple:bool) -> Result<Vec<std::path::PathBuf>> {
    unsafe {
        use windows::Win32::UI::Shell::*;
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL)?;
        let mut options=FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST;
        if multiple{options|=FOS_ALLOWMULTISELECT;}
        dialog.SetOptions(options)?;
        dialog.SetTitle(&HSTRING::from(title))?;
        if let Err(e) = dialog.Show(None) {
            if e.code().0 as u32 == 0x800704c7 {
                return Ok(vec![]);
            }
            return Err(e);
        }
        let items = dialog.GetResults()?;
        let mut out = vec![];
        for i in 0..items.GetCount()? {
            let ptr = items.GetItemAt(i)?.GetDisplayName(SIGDN_FILESYSPATH)?;
            let text = ptr.to_string()?;
            windows::Win32::System::Com::CoTaskMemFree(Some(ptr.0 as _));
            out.push(text.into());
        }
        Ok(out)
    }
}
/// 构建资源管理器可识别的文件数据对象。
fn file_data_object(path: &std::path::Path) -> Result<windows::Win32::System::Com::IDataObject> {
    unsafe {
        use windows::Win32::UI::Shell::*;
        let wide: Vec<u16> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let pidl = ILCreateFromPathW(windows::core::PCWSTR(wide.as_ptr()));
        if pidl.is_null() {
            return Err(windows::core::Error::from_win32());
        }
        let result = (|| {
            let mut child = std::ptr::null_mut();
            let folder: IShellFolder = SHBindToParent(pidl, Some(&mut child))?;
            folder.GetUIObjectOf(None, &[child as *const _], None)
        })();
        ILFree(Some(pidl));
        result
    }
}
/// 原生拖出只允许复制，取消拖放不会改变置物架与原文件。
pub fn drag_file(path: &std::path::Path) -> Result<()> {
    unsafe {
        use windows::Win32::System::Ole::{OleInitialize, OleUninitialize, DROPEFFECT_COPY};
        use windows::Win32::UI::Shell::SHDoDragDrop;
        let initialized = OleInitialize(None).is_ok();
        let result = file_data_object(path)
            .and_then(|data| SHDoDragDrop(None, &data, None, DROPEFFECT_COPY).map(|_| ()));
        if initialized {
            OleUninitialize();
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_app_names_hide_shortcut_suffix_and_fallback() {
        assert_eq!(clean_app_name("PowerShell 7 (x64).lnk"), "PowerShell 7 (x64)");
        assert_eq!(clean_app_name("  微信.lnk "), "微信");
        assert_eq!(clean_app_name("  "), "Windows 通知");
        assert_eq!(clean_app_name("Windows Shell Experience 主机"), "Windows Shell Experience 主机");
    }

    #[test]
    fn browser_minimal_recognizes_browsers_without_matching_electron_apps(){
        for name in ["chrome.exe","MSEDGE.EXE","firefox.exe","brave.exe","zen.exe","QQBrowser.exe","quark.exe","Quark.exe","QUARK.EXE"] {
            assert!(is_browser_process(name),"应识别浏览器 {name}");
        }
        for name in ["Codex.exe","Code.exe","msedgewebview2.exe","explorer.exe","chrome-helper.exe","quark_proxy.exe","quark_pwa_launcher.exe","quark_host_client.exe","quark_swap_util.exe","QuarkUpdaterSetup.exe",""] {
            assert!(!is_browser_process(name),"不能将 {name} 当作浏览器");
        }
    }
    #[test]
    fn browser_minimal_checks_the_actual_top_overlap_on_the_island_monitor(){
        let island=RECT{left:560,top:0,right:1360,bottom:238};
        for browser in [RECT{left:0,top:-8,right:1920,bottom:1200},RECT{left:0,top:0,right:960,bottom:1150},RECT{left:700,top:10,right:1500,bottom:800}] {
            assert!(browser_overlaps_island(browser,island));
        }
        for browser in [RECT{left:100,top:150,right:1700,bottom:1100},RECT{left:1920,top:0,right:3840,bottom:1200},RECT{left:0,top:0,right:600,bottom:1000},RECT{left:0,top:-1200,right:1920,bottom:0}] {
            assert!(!browser_overlaps_island(browser,island));
        }
    }
    #[test]
    fn network_connection_uses_connected_wording() {
        let old=Snapshot{volume:None,audio_endpoint:None,caps:false,network:false,network_kind:NetworkKind::Other};
        let mut connected=old.clone();connected.network=true;
        assert_eq!(connected.changes(&old),vec![("网络连接".into(),"网络已连接".into())]);
    }
    #[test]
    fn network_switches_transport_and_keeps_the_disconnected_type() {
        let wifi=Snapshot{volume:None,audio_endpoint:None,caps:false,network:true,network_kind:NetworkKind::Wifi};
        let mut cable=wifi.clone();cable.network_kind=NetworkKind::Ethernet;
        assert_eq!(cable.changes(&wifi),vec![("网络连接".into(),"以太网 · 网络已连接".into())]);
        let mut off=cable.clone();off.network=false;off.network_kind=NetworkKind::Other;
        assert_eq!(off.changes(&cable),vec![("网络连接".into(),"以太网 · 网络已断开".into())]);
        assert_eq!(wifi.changes(&off),vec![("网络连接".into(),"Wi-Fi · 网络已连接".into())]);
    }
    #[test]
    fn switching_audio_endpoints_is_not_a_volume_adjustment() {
        let old=Snapshot{volume:Some((20,false)),audio_endpoint:Some("speakers".into()),caps:false,network:true,network_kind:NetworkKind::Other};
        let mut new=old.clone();
        new.audio_endpoint=Some("headphones".into());new.volume=Some((80,false));new.caps=true;
        assert_eq!(new.changes(&old),vec![("大写锁定".into(),"已开启".into())]);
        let mut adjusted=new.clone();adjusted.volume=Some((60,false));
        assert_eq!(adjusted.changes(&new),vec![("系统音量".into(),"音量 60%".into())]);
        adjusted.volume=Some((60,true));
        assert_eq!(adjusted.changes(&new),vec![("系统音量".into(),"已静音".into())]);
        assert!(!old.changes(&new).iter().any(|(title,_)|title=="系统音量"));
    }

    #[test]
    fn dragged_file_exposes_native_file_drop_format() {
        unsafe {
            use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
            use windows::Win32::System::Ole::{OleInitialize, OleUninitialize, CF_HDROP};
            let initialized = OleInitialize(None).is_ok();
            let path =
                std::env::temp_dir().join(format!("daozai-drag-test-{}.txt", std::process::id()));
            std::fs::write(&path, "test").unwrap();
            {
                let data = file_data_object(&path).unwrap();
                let format = FORMATETC {
                    cfFormat: CF_HDROP.0,
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: -1,
                    tymed: TYMED_HGLOBAL.0 as u32,
                    ..Default::default()
                };
                assert!(data.QueryGetData(&format).is_ok());
            }
            std::fs::remove_file(path).unwrap();
            if initialized {
                OleUninitialize();
            }
        }
    }
}

fn avoidance_matches(path:&str,settings:&crate::model::Settings)->bool {
    settings.browser_minimal && (crate::avoidance::contains_app(&settings.avoid_apps,path)
        || settings.avoid_browsers && std::path::Path::new(path).file_name().and_then(|name|name.to_str()).is_some_and(is_browser_process))
}

pub(crate) fn avoidance_windows()->std::result::Result<Vec<crate::avoidance::Candidate>,String> {
    use windows::Win32::Foundation::{BOOL,LPARAM};
    unsafe extern "system" fn visit(hwnd:HWND,param:LPARAM)->BOOL {
        if !IsWindowVisible(hwnd).as_bool() || GetWindowLongPtrW(hwnd,GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0!=0 {return BOOL(1);}
        let mut cloaked=0u32;
        let _=windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(hwnd,windows::Win32::Graphics::Dwm::DWMWA_CLOAKED,(&mut cloaked as *mut u32).cast(),4);
        if cloaked!=0{return BOOL(1);}
        let mut title=[0u16;1024];let len=GetWindowTextW(hwnd,&mut title);
        if len==0{return BOOL(1);}
        let mut pid=0;GetWindowThreadProcessId(hwnd,Some(&mut pid));
        if pid==GetCurrentProcessId(){return BOOL(1);}
        let Ok(process)=OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,pid)else{return BOOL(1)};
        let mut path=[0u16;32768];let mut length=path.len() as u32;
        let result=QueryFullProcessImageNameW(process,PROCESS_NAME_WIN32,PWSTR(path.as_mut_ptr()),&mut length);
        let _=CloseHandle(process);
        if result.is_err(){return BOOL(1);}
        let path=String::from_utf16_lossy(&path[..length as usize]);
        let items=&mut *(param.0 as *mut Vec<crate::avoidance::Candidate>);
        if items.iter().any(|a|crate::avoidance::same_app(&a.path,&path)){return BOOL(1);}
        let filename=std::path::Path::new(&path).file_name().and_then(|s|s.to_str()).unwrap_or("");
        let name=executable_description(&path).unwrap_or_else(||filename.trim_end_matches(".exe").to_string());
        items.push(crate::avoidance::Candidate{browser:is_browser_process(filename),path,name,title:String::from_utf16_lossy(&title[..len as usize])});
        BOOL(1)
    }
    let mut items:Vec<crate::avoidance::Candidate>=Vec::new();
    unsafe{EnumWindows(Some(visit),LPARAM(&mut items as *mut _ as isize))}.map_err(|_|"无法读取应用列表，请刷新重试".to_string())?;
    items.sort_by_key(|a|a.name.to_lowercase());
    Ok(items)
}

fn executable_description(path:&str)->Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW,GetFileVersionInfoW,VerQueryValueW};
    let path:Vec<u16>=path.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let size=GetFileVersionInfoSizeW(PCWSTR(path.as_ptr()),None);if size==0{return None;}
        let mut bytes=vec![0u8;size as usize];GetFileVersionInfoW(PCWSTR(path.as_ptr()),0,size,bytes.as_mut_ptr().cast()).ok()?;
        let mut ptr=std::ptr::null_mut();let mut len=0;
        if !VerQueryValueW(bytes.as_ptr().cast(),windows::core::w!("\\VarFileInfo\\Translation"),&mut ptr,&mut len).as_bool() || len<4{return None;}
        let translation=std::slice::from_raw_parts(ptr as *const u16,2);
        let key:Vec<u16>=format!("\\StringFileInfo\\{:04x}{:04x}\\FileDescription",translation[0],translation[1]).encode_utf16().chain(Some(0)).collect();
        if !VerQueryValueW(bytes.as_ptr().cast(),PCWSTR(key.as_ptr()),&mut ptr,&mut len).as_bool() || len<=1{return None;}
        let name=String::from_utf16_lossy(std::slice::from_raw_parts(ptr as *const u16,len as usize)).trim_end_matches('\0').trim().to_string();
        (!name.is_empty()).then_some(name)
    }
}

#[cfg(test)]
mod avoidance_tests {
    use super::*;
    #[test]
    fn avoidance_respects_master_browser_switch_and_custom_app_identity() {
        let mut settings=crate::model::Settings::default();
        settings.avoid_apps=vec![(r"D:\Apps\Editor.exe".into(),"编辑器".into())];
        assert!(!avoidance_matches(r"D:\Apps\Editor.exe",&settings));
        settings.browser_minimal=true;
        assert!(avoidance_matches(r"D:\Quark\quark.exe",&settings));
        assert!(avoidance_matches(r"d:\apps\EDITOR.EXE",&settings));
        assert!(!avoidance_matches(r"C:\Other\Editor.exe",&settings));
        settings.avoid_browsers=false;
        assert!(!avoidance_matches(r"D:\Quark\quark.exe",&settings));
        assert!(avoidance_matches(r"D:\Apps\Editor.exe",&settings));
        settings.avoid_apps.clear();
        assert!(!avoidance_matches(r"D:\Apps\Editor.exe",&settings));
    }
    #[test]
    fn avoidance_window_list_excludes_this_process_and_deduplicates_apps() {
        let entries=avoidance_windows().unwrap();
        let current=std::env::current_exe().unwrap().to_string_lossy().into_owned();
        for (index,app) in entries.iter().enumerate() {
            assert!(!crate::avoidance::same_app(&app.path,&current));
            assert!(!app.name.is_empty() && !app.title.is_empty());
            assert!(!entries[..index].iter().any(|previous|crate::avoidance::same_app(&previous.path,&app.path)));
        }
        let notepad=std::path::Path::new(r"C:\Windows\System32\notepad.exe");
        if notepad.exists(){assert!(executable_description(notepad.to_str().unwrap()).is_some());}
    }
}

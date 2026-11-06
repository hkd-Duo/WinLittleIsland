//! 配色、字体与字号。
//!
//! 字体：拉丁/数字用 Segoe UI（Windows 原生 UI 字体，与原版 DirectWrite 渲染同源），
//! 中文回退到微软雅黑 UI。三档字重：常规 / 半粗 / 粗。
//! 微软雅黑不含日文假名/生僻字、韩文谚文、泰文、印度系、阿拉伯/希伯来/越南语字形，
//! 因此每档字重都在中文之后、egui 内置字体之前追加脚本回退：
//! 日文 Yu Gothic UI（常规/半粗/粗，回退 Yu Gothic / Meiryo / MS Gothic）、
//! 韩文 Malgun Gothic、泰文 Leelawadee UI、印度系 Nirmala UI、
//! 阿拉伯/希伯来/越南语 Tahoma。系统缺失的字体自动跳过。
//! 非 Windows（开发机、录屏）按 `DAOZAI_FONT_DIR` → 常见系统路径依次查找
//! 度量兼容的 Selawik 与 Noto Sans SC。

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------- 颜色（原版逐像素取色，少量为审美微调） ----------
pub const RED: Color32 = Color32::from_rgb(242, 48, 70);
pub const RED_TOP: Color32 = Color32::from_rgb(253, 104, 92);
pub const BLUE: Color32 = Color32::from_rgb(62, 172, 248);
pub const BLUE_DEEP: Color32 = Color32::from_rgb(13, 69, 128);
pub const CYAN: Color32 = Color32::from_rgb(102, 219, 255);
pub const SOFT_GREEN: Color32 = Color32::from_rgb(118, 196, 151);
pub const GREEN: Color32 = Color32::from_rgb(48, 209, 88);
pub const CHARGE: Color32 = Color32::from_rgb(33, 235, 84);
pub const WHITE: Color32 = Color32::WHITE;
pub const GRAY: Color32 = Color32::from_rgb(178, 178, 186);
pub const DIM: Color32 = Color32::from_rgb(142, 142, 153);
pub const BTN: Color32 = Color32::from_rgb(40, 40, 43);
pub const BTN_HOVER: Color32 = Color32::from_rgb(56, 56, 60);
pub const CARD: Color32 = Color32::from_rgb(22, 22, 24);
pub const TRACK: Color32 = Color32::from_rgb(36, 36, 39);
pub const ARROW_BG: Color32 = Color32::from_rgb(48, 48, 51);
pub const WATER_TRACK: Color32 = Color32::from_rgb(15, 38, 64);
pub const YELLOW: Color32 = Color32::from_rgb(255, 196, 61);

pub fn alpha_blue(a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(BLUE.r(), BLUE.g(), BLUE.b(), (255.0 * a) as u8)
}

/// 字重档位。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum W {
    Regular,
    Semibold,
    Bold,
}

pub fn font(size: f32, w: W) -> FontId {
    match w {
        W::Regular => FontId::proportional(size),
        W::Semibold => FontId::new(size, FontFamily::Name("semibold".into())),
        W::Bold => FontId::new(size, FontFamily::Name("bold".into())),
    }
}

/// 字号表（逻辑点）。汉字天然比拉丁字母「显大」，中文字号单独一套，
/// 让两种文字的视觉重量一致，而不是硬把墨高压到拉丁大写高度。
pub mod size {
    pub const TITLE: f32 = 14.5; // 页标题（中文）
    pub const DATE: f32 = 11.0; // 顶栏日期
    pub const CLOCK: f32 = 12.5; // 顶栏时钟
    pub const PILL_CLOCK: f32 = 15.5; // 药丸时钟
    pub const HERO: f32 = 29.0; // 大数字（25:00 / 0.1 / 0/0）
    pub const BODY: f32 = 11.5; // 正文、按钮
    pub const SMALL: f32 = 11.0; // 说明
    pub const TINY: f32 = 10.0; // 脚注、坐标轴
}

struct Face {
    path: PathBuf,
    index: u32,
}

fn find(cands: &[(&str, u32)], dirs: &[PathBuf]) -> Option<Face> {
    for (name, index) in cands {
        let p = Path::new(name);
        if p.is_absolute() && p.exists() {
            return Some(Face { path: p.to_path_buf(), index: *index });
        }
        for d in dirs {
            let q = d.join(name);
            if q.exists() {
                return Some(Face { path: q, index: *index });
            }
        }
    }
    None
}

fn font_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(d) = std::env::var("DAOZAI_FONT_DIR") {
        v.push(PathBuf::from(d));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            v.push(d.join("fonts"));
        }
    }
    v.push(PathBuf::from("assets/fonts"));
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    v.push(PathBuf::from(windir).join("Fonts"));
    for d in [
        "/usr/share/fonts/google-noto-cjk",
        "/usr/share/fonts/opentype/noto",
        "/usr/share/fonts/noto-cjk",
        "/usr/share/fonts/truetype",
    ] {
        v.push(PathBuf::from(d));
    }
    v
}

/// 装载三档字重。每档 = 拉丁字体 + 中文回退 + 各脚本回退；缺哪个就跳过，最后兜底 egui 内置字体。
pub fn load_fonts(ctx: &egui::Context) {
    ctx.set_fonts(shared_fonts().clone());
}

fn shared_fonts()->&'static FontDefinitions {
    static FONTS:std::sync::OnceLock<FontDefinitions>=std::sync::OnceLock::new();
    FONTS.get_or_init(build_fonts)
}

#[cfg(windows)]
struct MappedFont {
    _source:std::fs::File,
    bytes:memmap2::Mmap,
}

#[cfg(windows)]
impl MappedFont {
    fn open(path:&Path)->std::io::Result<Self>{
        use std::os::windows::fs::OpenOptionsExt;
        // 保留文件句柄并禁止原地写入；允许系统以替换文件的方式更新字体。
        let source=std::fs::OpenOptions::new().read(true).share_mode(0x1|0x4).open(path)?;
        let bytes=unsafe{memmap2::Mmap::map(&source)?};
        Ok(Self{_source:source,bytes})
    }
}

fn font_bytes(path:&Path)->Option<&'static [u8]>{
    #[cfg(windows)]
    if let Ok(mapped)=MappedFont::open(path){
        // 所有字体由进程级 OnceLock 持有，映射和只读句柄与排版器同寿命。
        let mapped=Box::leak(Box::new(mapped));return Some(&mapped.bytes);
    }
    Some(Box::leak(std::fs::read(path).ok()?.into_boxed_slice()))
}

fn build_fonts()->FontDefinitions {
    let dirs = font_dirs();
    let latin = [
        find(&[("segoeui.ttf", 0), ("selawk.ttf", 0)], &dirs),
        find(&[("seguisb.ttf", 0), ("selawksb.ttf", 0), ("segoeuib.ttf", 0)], &dirs),
        find(&[("segoeuib.ttf", 0), ("selawkb.ttf", 0)], &dirs),
    ];
    let cjk = [
        find(&[("msyh.ttc", 1), ("NotoSansSC-Regular.otf", 0), ("NotoSansCJK-Regular.ttc", 2)], &dirs),
        find(&[("msyhbd.ttc", 1), ("NotoSansSC-Medium.otf", 0), ("NotoSansCJK-Medium.ttc", 2)], &dirs),
        find(&[("msyhbd.ttc", 1), ("NotoSansSC-Bold.otf", 0), ("NotoSansCJK-Bold.ttc", 2)], &dirs),
    ];
    // 脚本回退链（每档字重独立）：微软雅黑不含下列字形，必须在 builtin 之前补齐。
    // 每项按优先级排列，缺失的字体由 find() 返回 None 并自动跳过。
    let jp = [
        vec![
            find(&[("YuGothM.ttc", 1)], &dirs), // Yu Gothic UI Regular
            find(&[("YuGothR.ttc", 0)], &dirs),
            find(&[("meiryo.ttc", 0)], &dirs),
            find(&[("msgothic.ttc", 0)], &dirs),
        ],
        vec![
            find(&[("YuGothB.ttc", 2)], &dirs), // Yu Gothic UI Semibold
            find(&[("YuGothM.ttc", 0)], &dirs),
            find(&[("YuGothR.ttc", 0)], &dirs),
        ],
        vec![
            find(&[("YuGothB.ttc", 1)], &dirs), // Yu Gothic UI Bold
            find(&[("YuGothB.ttc", 0)], &dirs),
        ],
    ];
    let kr = [
        vec![find(&[("malgun.ttf", 0)], &dirs)],
        vec![find(&[("malgunbd.ttf", 0)], &dirs), find(&[("malgun.ttf", 0)], &dirs)],
        vec![find(&[("malgunbd.ttf", 0)], &dirs), find(&[("malgun.ttf", 0)], &dirs)],
    ];
    let th = [
        vec![find(&[("LeelawUI.ttf", 0)], &dirs)],
        vec![find(&[("LeelaUIb.ttf", 0)], &dirs), find(&[("LeelawUI.ttf", 0)], &dirs)],
        vec![find(&[("LeelaUIb.ttf", 0)], &dirs), find(&[("LeelawUI.ttf", 0)], &dirs)],
    ];
    let indic = [
        vec![find(&[("Nirmala.ttc", 0)], &dirs)],
        vec![find(&[("Nirmala.ttc", 1)], &dirs)],
        vec![find(&[("Nirmala.ttc", 1)], &dirs)],
    ];
    let arab = [
        vec![find(&[("tahoma.ttf", 0)], &dirs)],
        vec![find(&[("tahomabd.ttf", 0)], &dirs), find(&[("tahoma.ttf", 0)], &dirs)],
        vec![find(&[("tahomabd.ttf", 0)], &dirs), find(&[("tahoma.ttf", 0)], &dirs)],
    ];

    let mut fonts = FontDefinitions::default();
    let mut cache: Vec<(PathBuf, u32, String)> = Vec::new();
    let mut add = |fonts: &mut FontDefinitions, face: &Option<Face>, key: String| -> Option<String> {
        let face = face.as_ref()?;
        if let Some((_, _, k)) = cache.iter().find(|(p, i, _)| *p == face.path && *i == face.index) {
            return Some(k.clone());
        }
        let mut d = FontData::from_static(font_bytes(&face.path)?);
        d.index = face.index;
        fonts.font_data.insert(key.clone(), Arc::new(d));
        cache.push((face.path.clone(), face.index, key.clone()));
        Some(key)
    };

    let builtin = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let families = [
        FontFamily::Proportional,
        FontFamily::Name("semibold".into()),
        FontFamily::Name("bold".into()),
    ];
    for (i, fam) in families.iter().enumerate() {
        let mut list = Vec::new();
        if let Some(k) = add(&mut fonts, &latin[i], format!("latin{i}")) {
            list.push(k);
        }
        if let Some(k) = add(&mut fonts, &cjk[i], format!("cjk{i}")) {
            list.push(k);
        }
        // 脚本回退（日文 → 韩文 → 泰文 → 印度系 → 阿拉伯/希伯来/越南语），
        // 必须排在 builtin 之前：egui 内置字体不含这些脚本，提前兜底会把它当成缺字方框。
        for (faces, prefix) in [
            (&jp[i], "jp"),
            (&kr[i], "kr"),
            (&th[i], "th"),
            (&indic[i], "indic"),
            (&arab[i], "arab"),
        ] {
            for (n, face) in faces.iter().enumerate() {
                // 首选项用 jp0/jp1/jp2，其余回退依次用 jp0_1、jp0_2…，保证 font_data key 唯一。
                let key = if n == 0 { format!("{prefix}{i}") } else { format!("{prefix}{i}_{n}") };
                if let Some(k) = add(&mut fonts, face, key) {
                    list.push(k);
                }
            }
        }
        list.extend(builtin.iter().cloned());
        fonts.families.insert(fam.clone(), list);
    }
    fonts
}

/// 各个原生窗口统一使用岛仔品牌图标，避免回落到框架默认图标。
pub fn app_icon()->std::sync::Arc<eframe::egui::IconData>{
    static ICON:std::sync::OnceLock<std::sync::Arc<eframe::egui::IconData>>=std::sync::OnceLock::new();
    ICON.get_or_init(||{
        let image=image::load_from_memory(include_bytes!("../assets/daozai-logo.png")).expect("岛仔图标资源应有效").thumbnail(128,128).to_rgba8();
        std::sync::Arc::new(eframe::egui::IconData{width:image.width(),height:image.height(),rgba:image.into_raw()})
    }).clone()
}

#[cfg(test)]
mod tests {
    use super::{find, font, font_dirs, load_fonts, shared_fonts, W};
    use eframe::egui::{self, FontDefinitions, FontFamily};

    /// 脚本回退字体是否存在（复用 build_fonts 的查找顺序，跨平台安全）。
    fn script_available(cands: &[(&str, u32)]) -> bool {
        find(cands, &font_dirs()).is_some()
    }

    /// 字体列表中 egui 内置字体段的起始下标。
    fn builtin_start(list: &[String]) -> usize {
        let defaults = FontDefinitions::default();
        let default_list = &defaults.families[&FontFamily::Proportional];
        list.iter().position(|k| default_list.contains(k)).unwrap_or(list.len())
    }

    #[test]
    fn script_fallbacks_are_inserted_between_cjk_and_builtin() {
        let fonts = shared_fonts();
        let prop = &fonts.families[&FontFamily::Proportional];
        assert!(
            prop.len() >= FontDefinitions::default().families[&FontFamily::Proportional].len(),
            "脚本回退不能挤掉 egui 内置字体"
        );
        let start = builtin_start(prop);
        // 拉丁仍是首选，中文紧随其后；脚本回退全部在 egui 内置字体之前。
        if fonts.font_data.contains_key("latin0") {
            assert_eq!(prop.first().map(String::as_str), Some("latin0"), "拉丁字体必须排在首位");
        }
        if fonts.font_data.contains_key("cjk0") {
            let pos = prop.iter().position(|k| k == "cjk0").expect("cjk0 必须出现在 Proportional 字体链中");
            if fonts.font_data.contains_key("latin0") {
                assert_eq!(pos, 1, "中文字体必须紧随拉丁字体");
            }
            assert!(pos < start, "中文字体必须在 egui 内置字体之前");
        }
        for (i, key) in prop.iter().enumerate() {
            let is_script = ["jp", "kr", "th", "indic", "arab"].iter().any(|p| key.starts_with(p));
            if is_script {
                assert!(i < start, "脚本回退 {key} 排在了 egui 内置字体之后，缺字方框会重新出现");
            }
        }
        // 系统字体存在时，常规档对应的回退 key 必须真实排在 builtin 之前。
        let regular: [(&[(&str, u32)], &str); 4] = [
            (&[("malgun.ttf", 0)], "kr0"),
            (&[("LeelawUI.ttf", 0)], "th0"),
            (&[("Nirmala.ttc", 0)], "indic0"),
            (&[("tahoma.ttf", 0)], "arab0"),
        ];
        for (cands, key) in regular {
            if !script_available(cands) {
                continue;
            }
            let pos = prop.iter().position(|k| k == key).unwrap_or_else(|| panic!("缺少回退 key {key}"));
            assert!(pos < start, "{key} 必须排在 egui 内置字体之前");
        }
        // 日文常规档优先级：YuGothM.ttc#1 → YuGothR.ttc#0 → meiryo.ttc#0 → msgothic.ttc#0。
        let jp_regular: &[(&[(&str, u32)], &str)] = &[
            (&[("YuGothM.ttc", 1)], "jp0"),
            (&[("YuGothR.ttc", 0)], "jp0_1"),
            (&[("meiryo.ttc", 0)], "jp0_2"),
            (&[("msgothic.ttc", 0)], "jp0_3"),
        ];
        if let Some((_, key)) = jp_regular.iter().find(|(cands, _)| script_available(cands)) {
            let pos = prop.iter().position(|k| k.as_str() == *key).unwrap_or_else(|| panic!("缺少日文回退 key {key}"));
            assert!(pos < start, "{key} 必须排在 egui 内置字体之前");
        }
    }

    #[test]
    fn script_fallbacks_cover_non_latin_glyphs() {
        let ctx = egui::Context::default();
        load_fonts(&ctx);
        // egui 在首帧排版时才真正创建字体实例。
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let semibold = font(11.0, W::Semibold);
        // (候选字体, 样例文本, 说明)：本机缺字体时跳过，CI 未装语言包也不会失败。
        let cases: &[(&[(&str, u32)], &str, &str)] = &[
            (&[("malgun.ttf", 0)], "안녕하세요", "韩文"),
            (&[("YuGothM.ttc", 1), ("YuGothR.ttc", 0), ("meiryo.ttc", 0), ("msgothic.ttc", 0)], "髙﨑邉", "日文生僻字"),
            (&[("LeelawUI.ttf", 0)], "สวัสดี", "泰文"),
            (&[("Nirmala.ttc", 0)], "नमस्ते", "印地文"),
            (&[("tahoma.ttf", 0)], "مرحبا", "阿拉伯文"),
            (&[("tahoma.ttf", 0)], "שלום", "希伯来文"),
            (&[("tahoma.ttf", 0)], "Tiếng Việt", "越南语"),
        ];
        for (cands, sample, label) in cases {
            if !script_available(cands) {
                continue;
            }
            assert!(ctx.fonts(|f| f.has_glyphs(&semibold, sample)), "{label}样例 {sample:?} 仍然缺字");
        }
    }

    #[cfg(windows)]
    #[test]
    fn mapped_fonts_prevent_in_place_writes_and_keep_shared_reads(){
        let path=std::env::temp_dir().join(format!("daozai-font-map-{}-{}.bin",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::write(&path,b"readonly font fixture").unwrap();
        let mapped=super::MappedFont::open(&path).unwrap();
        assert_eq!(&mapped.bytes[..],b"readonly font fixture");
        assert_eq!(std::fs::read(&path).unwrap(),b"readonly font fixture");
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err(),"映射存活期间不能允许原地改写字体");
        drop(mapped);std::fs::remove_file(path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn system_font_bytes_use_read_only_file_mapping(){
        #[link(name="kernel32")]
        unsafe extern "system"{
            fn K32GetMappedFileNameW(process:*mut core::ffi::c_void,address:*const core::ffi::c_void,buffer:*mut u16,size:u32)->u32;
        }
        let fonts=super::shared_fonts();let font=&fonts.font_data["cjk0"];
        let mut name=[0u16;1024];
        let count=unsafe{K32GetMappedFileNameW(windows::Win32::System::Threading::GetCurrentProcess().0,font.font.as_ptr().cast(),name.as_mut_ptr(),name.len()as u32)};
        assert!(count>0,"中文字体仍占用完整的私有堆副本，没有复用系统字体文件页");
        let name=String::from_utf16_lossy(&name[..count as usize]).to_lowercase();
        assert!([".ttc",".ttf",".otf"].iter().any(|suffix|name.ends_with(suffix)),"映射必须来自实际字体文件");
    }

    #[test]
    fn font_data_is_shared_across_contexts_without_owned_byte_copies(){
        let fonts=super::shared_fonts();
        assert!(fonts.font_data.values().all(|font|matches!(font.font,std::borrow::Cow::Borrowed(_))),"字体不能在每个排版上下文中产生完整副本");
        let cloned=fonts.clone();
        for (name,font) in &fonts.font_data {assert!(std::sync::Arc::ptr_eq(font,&cloned.font_data[name]));}
    }
}

//! 系统媒体会话（SMTC）读取：Windows 10/11 的“正在播放”。
//! 与原工具使用同一套 API（GlobalSystemMediaTransportControlsSessionManager）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MediaKind {
    Music,
    Video,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub source: String,
    pub playing: bool,
    pub position: f64,
    /// 播放位置的采样时刻，避免封面读取和线程交接消耗歌词提前量。
    pub sampled_at: Option<std::time::Instant>,
    pub duration: f64,
    pub artwork: Option<Arc<eframe::egui::ColorImage>>,
    pub playback_type: MediaKind,
}

impl NowPlaying {
    pub fn kind(&self)->MediaKind {
        if self.title.trim().is_empty(){return MediaKind::Unknown;}
        if self.playback_type==MediaKind::Video {return MediaKind::Video;}
        // 部分浏览器把所有媒体都上报为音乐，补充识别明确的视频页面标题。
        let title=self.title.trim().to_lowercase();
        if [" - 抖音"," - youtube","_哔哩哔哩_bilibili"," - 哔哩哔哩"].iter().any(|suffix|title.ends_with(suffix)) {
            return MediaKind::Video;
        }
        let source=self.source.to_lowercase();
        if ["chrome","msedge","firefox","brave","opera"].iter().any(|name|source.contains(name)) {
            let album=self.album.trim().to_lowercase();
            if ["youtube","bilibili","抖音","哔哩哔哩"].contains(&album.as_str()){return MediaKind::Video;}
            // 视频作者也可能被放进 Artist；浏览器缺少专辑信息时不自动认作歌曲。
            if album.is_empty(){return MediaKind::Unknown;}
        }
        if self.artist.trim().is_empty(){MediaKind::Unknown}else{MediaKind::Music}
    }

    pub fn is_music(&self)->bool {self.kind()==MediaKind::Music}
}

#[derive(Default)]
struct ArtworkCache {
    key: String,
    bytes: Vec<u8>,
    image: Option<Arc<eframe::egui::ColorImage>>,
}

impl ArtworkCache {
    fn get(&mut self, key:String, load:impl FnOnce()->Option<Vec<u8>>)->Option<Arc<eframe::egui::ColorImage>> {
        if self.key!=key {self.key=key;self.bytes.clear();self.image=None;}
        // 封面可能晚于歌名更新；每次检查内容，只在图片变化时重新解码。
        if let Some(bytes)=load() {
            if bytes!=self.bytes {
                if let Some(image)=decode_artwork(&bytes) {
                    if self.image.as_deref()!=Some(&image){self.image=Some(Arc::new(image));}
                }
                self.bytes=bytes;
            }
        }
        self.image.clone()
    }
}

fn decode_artwork(bytes:&[u8])->Option<eframe::egui::ColorImage> {
    let image=image::load_from_memory(bytes).ok()?.thumbnail(256,256).to_rgba8();
    Some(eframe::egui::ColorImage::from_rgba_unmultiplied([image.width() as usize,image.height() as usize],image.as_raw()))
}

#[cfg(windows)]
fn read_artwork(props:&windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties)->Option<Vec<u8>> {
    use windows::Storage::Streams::DataReader;
    let stream=props.Thumbnail().ok()?.OpenReadAsync().ok()?.get().ok()?;
    let size=stream.Size().ok()?;
    if size==0 || size>8*1024*1024 {return None;}
    let reader=DataReader::CreateDataReader(&stream.GetInputStreamAt(0).ok()?).ok()?;
    let count=reader.LoadAsync(size as u32).ok()?.get().ok()?;
    if count!=size as u32 {return None;}
    let mut bytes=vec![0;count as usize];
    reader.ReadBytes(&mut bytes).ok()?;
    Some(bytes)
}

pub type Shared = Arc<Mutex<NowPlaying>>;

pub fn shared() -> Shared {
    Arc::new(Mutex::new(NowPlaying::default()))
}

#[cfg(windows)]
fn query(artwork:&mut ArtworkCache) -> Option<NowPlaying> {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;

    let mgr = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
        .ok()?
        .get()
        .ok()?;
    let session = mgr.GetCurrentSession().ok()?;
    let props = session.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
    let title = props.Title().map(|s| s.to_string()).unwrap_or_default();
    let artist = props.Artist().map(|s| s.to_string()).unwrap_or_default();
    if title.is_empty() {
        return None;
    }
    let mut playing = false;
    let mut position = 0.0;
    let mut duration = 0.0;
    let mut sampled_at=None;
    let mut playback_type=props.PlaybackType().ok().and_then(|kind|kind.Value().ok());
    if let Ok(info) = session.GetPlaybackInfo() {
        let info_type=info.PlaybackType().ok().and_then(|kind|kind.Value().ok());
        if playback_type.is_none() || info_type.is_some_and(|kind|kind.0==2){playback_type=info_type;}
        if let Ok(status) = info.PlaybackStatus() {
            playing = status == windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing;
        }
    }
    if let Ok(tl) = session.GetTimelineProperties() {
        sampled_at=Some(std::time::Instant::now());
        if let Ok(p) = tl.Position() {
            position = p.Duration as f64 / 10_000_000.0;
        }
        // SMTC 只在事件发生时更新 Position；播放中要按 LastUpdatedTime 外推到「现在」。
        if playing {
            if let Ok(at) = tl.LastUpdatedTime() {
                let now = unsafe { windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime() };
                let now = ((now.dwHighDateTime as i64) << 32) | now.dwLowDateTime as i64;
                let lag = (now - at.UniversalTime) as f64 / 10_000_000.0;
                if (0.0..86_400.0).contains(&lag) {
                    position += lag;
                }
            }
        }
        if let Ok(e) = tl.EndTime() {
            duration = e.Duration as f64 / 10_000_000.0;
        }
    }
    let source=session.SourceAppUserModelId().map(|s|s.to_string()).unwrap_or_default();
    let album=props.AlbumTitle().map(|s|s.to_string()).unwrap_or_default();
    let key=format!("{source}|{title}|{artist}|{album}");
    let artwork=artwork.get(key,||read_artwork(&props));
    let playback_type=match playback_type.map(|kind|kind.0){Some(1)=>MediaKind::Music,Some(2)=>MediaKind::Video,_=>MediaKind::Unknown};
    Some(NowPlaying { title, artist, album, source, playing, position, sampled_at, duration, artwork, playback_type })
}

#[cfg(not(windows))]
fn query(_artwork:&mut ArtworkCache) -> Option<NowPlaying> {
    None
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Transport {
    Toggle,
    Next,
    Prev,
    /// 跳到第几秒。
    Seek(f64),
}

/// 控制当前媒体会话（播放/暂停、上一首、下一首）。
#[cfg(windows)]
pub fn control(cmd: Transport) {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;

    let Ok(op) = GlobalSystemMediaTransportControlsSessionManager::RequestAsync() else {
        return;
    };
    let Ok(mgr) = op.get() else { return };
    let Ok(session) = mgr.GetCurrentSession() else { return };
    let result = match cmd {
        Transport::Toggle => session.TryTogglePlayPauseAsync(),
        Transport::Next => session.TrySkipNextAsync(),
        Transport::Prev => session.TrySkipPreviousAsync(),
        Transport::Seek(secs) => session.TryChangePlaybackPositionAsync((secs.max(0.0) * 10_000_000.0) as i64),
    };
    if let Ok(op) = result {
        let _ = op.get();
    }
}

#[cfg(not(windows))]
pub fn control(_cmd: Transport) {}

#[cfg(windows)]
fn init_winrt() {
    use windows::Win32::System::Com::CoIncrementMTAUsage;
    unsafe {
        let _ = CoIncrementMTAUsage();
    }
}

#[cfg(not(windows))]
fn init_winrt() {}

/// 后台线程：每秒刷新一次正在播放（进度条在两次刷新之间由界面按时间外推）。
pub fn spawn_poller(state: Shared,ctx:eframe::egui::Context) {
    std::thread::spawn(move || {
        init_winrt();
        let mut artwork=ArtworkCache::default();
        loop {
            let np = query(&mut artwork).unwrap_or_default();
            if np.title.is_empty() {artwork=ArtworkCache::default();}
            if let Ok(mut cur) = state.lock() {
                let changed=cur.title!=np.title || cur.artist!=np.artist || cur.playing!=np.playing
                    || cur.source!=np.source || cur.duration!=np.duration
                    || cur.artwork.as_ref().zip(np.artwork.as_ref()).map_or(cur.artwork.is_some()!=np.artwork.is_some(),|(old,new)|!Arc::ptr_eq(old,new));
                *cur = np;
                if changed{ctx.request_repaint();}
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cover_bytes(color:eframe::egui::Color32)->Vec<u8> {
        let image=image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(2,2,image::Rgba(color.to_array())));
        let mut bytes=std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes,image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    #[test]
    fn media_kind_checks_video_type_before_song_metadata(){
        let mut np=NowPlaying{title:"Video title".into(),artist:"Video author".into(),playback_type:MediaKind::Video,..Default::default()};
        assert_eq!(np.kind(),MediaKind::Video);
        np.playback_type=MediaKind::Music;np.title="小唐的Token的抖音 - 抖音".into();
        assert_eq!(np.kind(),MediaKind::Video,"浏览器错误上报音乐类型时仍应识别明确的视频标题");
        np.title="Movie - YouTube".into();assert_eq!(np.kind(),MediaKind::Video);
        np.title="阿拉斯加海湾".into();np.artist="蓝心羽".into();assert!(np.is_music());
        np.artist.clear();assert_eq!(np.kind(),MediaKind::Unknown,"只有页面标题不能确认是歌曲");
    }

    #[test]
    fn browser_video_authors_are_not_enough_to_identify_a_song(){
        let mut np=NowPlaying{title:"A video".into(),artist:"Video creator".into(),source:"Chrome".into(),playback_type:MediaKind::Music,..Default::default()};
        assert_eq!(np.kind(),MediaKind::Unknown);
        np.album="YouTube".into();assert_eq!(np.kind(),MediaKind::Video);
        np.title="阿拉斯加海湾".into();np.artist="蓝心羽".into();np.album="阿拉斯加海湾".into();
        assert_eq!(np.kind(),MediaKind::Music,"网易云网页版的完整歌曲元数据仍应识别为音乐");
        np.source="Spotify.exe".into();np.album.clear();assert_eq!(np.kind(),MediaKind::Music,"音乐客户端不强制要求专辑信息");
    }

    #[test]
    fn artwork_cache_retries_late_images_and_clears_on_song_change(){
        let mut cache=ArtworkCache::default();
        assert!(cache.get("first".into(),||None).is_none());
        let first=cache.get("first".into(),||Some(cover_bytes(eframe::egui::Color32::RED))).unwrap();
        let cached=cache.get("first".into(),||Some(cover_bytes(eframe::egui::Color32::RED))).unwrap();
        assert!(Arc::ptr_eq(&first,&cached));
        let missing=cache.get("first".into(),||None).unwrap();
        assert!(Arc::ptr_eq(&first,&missing),"短暂缺失时应保留当前歌曲封面");
        let invalid=cache.get("first".into(),||Some(b"invalid image".to_vec())).unwrap();
        assert!(Arc::ptr_eq(&first,&invalid),"损坏的缩略图不应清空当前有效封面");
        assert!(cache.get("second".into(),||None).is_none(),"切歌后不能保留上一首封面");
    }

    #[test]
    fn artwork_cache_replaces_a_temporary_cover_on_the_same_song(){
        let mut cache=ArtworkCache::default();
        let temporary=cache.get("song".into(),||Some(cover_bytes(eframe::egui::Color32::RED))).unwrap();
        // 网页可能先提供占位图，随后才发布真实封面。
        let actual=cache.get("song".into(),||Some(cover_bytes(eframe::egui::Color32::BLUE))).unwrap();
        assert!(!Arc::ptr_eq(&temporary,&actual),"同一首歌的真实封面到达后必须替换占位图");
        assert_eq!(actual.pixels[0],eframe::egui::Color32::BLUE);
    }

    #[test]
    fn artwork_decodes_png_jpeg_webp_and_rejects_invalid_bytes(){
        let original=image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(600,300,image::Rgb([24,120,210])));
        for format in [image::ImageFormat::Png,image::ImageFormat::Jpeg,image::ImageFormat::WebP] {
            let mut bytes=std::io::Cursor::new(Vec::new());
            original.write_to(&mut bytes,format).unwrap();
            let decoded=decode_artwork(bytes.get_ref()).unwrap();
            assert_eq!(decoded.size,[256,128]);
            assert!(decoded.pixels.iter().all(|p|p.a()==255));
        }
        assert!(decode_artwork(b"invalid image").is_none());
    }

    #[cfg(windows)]
    #[test]
    #[ignore="需要读取当前系统媒体会话"]
    fn live_sessions_metadata(){
        init_winrt();
        let manager=windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager::RequestAsync().unwrap().get().unwrap();
        for session in manager.GetSessions().unwrap() {
            let props=session.TryGetMediaPropertiesAsync().unwrap().get().unwrap();
            println!("source={} title={} artist={} type={:?} state={:?} duration={:?}",session.SourceAppUserModelId().unwrap(),props.Title().unwrap(),props.Artist().unwrap(),props.PlaybackType().ok().and_then(|kind|kind.Value().ok()),session.GetPlaybackInfo().unwrap().PlaybackStatus(),session.GetTimelineProperties().unwrap().EndTime());
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore="需要当前系统有提供封面的媒体会话"]
    fn live_current_session_artwork(){
        init_winrt();
        let np=query(&mut ArtworkCache::default()).expect("当前没有媒体会话");
        println!("当前媒体：{} / {}；来源 {}；类型 {:?}",np.title,np.artist,np.source,np.kind());
        let cover=np.artwork.expect("当前媒体会话没有可读封面");
        println!("系统封面尺寸：{}×{}",cover.size[0],cover.size[1]);
        if let Ok(path)=std::env::var("DAOZAI_ARTWORK_PROBE") {
            let pixels:Vec<u8>=cover.pixels.iter().flat_map(|p|p.to_srgba_unmultiplied()).collect();
            image::save_buffer(path,&pixels,cover.size[0] as u32,cover.size[1] as u32,image::ColorType::Rgba8).unwrap();
        }
    }
}

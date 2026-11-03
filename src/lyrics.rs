//! 歌词：按歌曲信息匹配网易云与 LRCLIB 的同步歌词，并缓存成功结果。

#[derive(Clone, Debug)]
pub struct LyricsLine {
    pub t: f32,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct Lyrics {
    pub lines: Vec<LyricsLine>,
    pub synced: bool,
}

#[derive(Clone,Debug,PartialEq,Eq)]
pub enum FetchError {
    Unavailable(String),
    NotFound,
    InvalidResponse(String),
    IncompleteMetadata,
}
impl FetchError {
    pub fn label(&self)->&'static str {
        match self {
            Self::Unavailable(_)=>"歌词服务暂不可用",
            Self::NotFound=>"未找到匹配歌词",
            Self::InvalidResponse(_)=>"歌词响应异常",
            Self::IncompleteMetadata=>"网页未提供完整歌曲信息",
        }
    }
}

impl Lyrics {
    /// 返回当前时间点应显示的行号。
    pub fn index_at(&self, secs: f32) -> Option<usize> {
        self.lines.iter().rposition(|line|line.t<=secs)
    }
}

fn parse_time(tag: &str) -> Option<f32> {
    // mm:ss.xx 或 mm:ss:xx
    let mut parts = tag.split(':');
    let m: u32 = parts.next()?.trim().parse().ok()?;
    let rest = parts.next()?.trim();
    let seconds=match parts.next() {Some(fraction)=>format!("{rest}.{fraction}"),None=>rest.replace(',', ".")};
    if parts.next().is_some(){return None;}
    let s: f32 = seconds.parse().ok()?;
    if !s.is_finite() || !(0.0..60.0).contains(&s){return None;}
    Some(m as f32 * 60.0 + s)
}

pub fn parse_lrc(raw: &str) -> Lyrics {
    let mut lines = Vec::new();
    for line in raw.lines() {
        let mut times = Vec::new();
        let mut text = line;
        while text.starts_with('[') {
            let start=0;
            let Some(end_rel) = text[start..].find(']') else { break };
            let end = start + end_rel;
            let tag = &text[start + 1..end];
            if let Some(t) = parse_time(tag) {
                times.push(t);
            } else if !times.is_empty() { break; }
            text = &text[end + 1..];
        }
        let body = text.trim();
        if body.is_empty() && times.is_empty() {
            continue;
        }
        for t in times {
            lines.push(LyricsLine { t, text: body.to_string() });
        }
    }
    lines.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
    let synced = !lines.is_empty();
    Lyrics { lines, synced }
}

#[test]
fn quality_lyrics_keep_brackets_in_the_actual_text() {
    let parsed=parse_lrc("[00:01.00]唱到[副歌]这里\n[00:02.00][00:03.00]下一句");
    assert_eq!(parsed.lines[0].text,"唱到[副歌]这里");
    assert_eq!(parsed.lines.len(),3);
}

/// 纯文本没有同步时间，保留正文但不伪造逐行时间戳。
pub fn from_plain(raw: &str) -> Lyrics {
    let lines: Vec<LyricsLine> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| LyricsLine { t: 0.0, text: l.trim().to_string() })
        .collect();
    Lyrics { lines, synced: false }
}

#[path="soda_cache.rs"]
mod soda_cache;

pub fn is_soda(source:&str)->bool {source=="汽水音乐" || source.to_ascii_lowercase().contains("sodamusic")}

/// 原生缓存优先；未缓存的新歌仍可使用已有在线来源。
pub fn fetch_for_source(source:&str,track:&str,artist:&str,duration:f64)->Result<Lyrics,FetchError> {
    if is_soda(source) {
        if let Some(raw)=soda_cache::read(track,artist,duration) {
            let lyrics=parse_soda(&raw);
            if lyrics.synced {return Ok(lyrics);}
        }
    }
    fetch(track,artist,duration)
}

fn parse_soda(raw:&str)->Lyrics {
    let line_tag=regex::Regex::new(r"^\[(\d+),(\d+)\]").unwrap();
    let word_tag=regex::Regex::new(r"<\d+,\d+,\d+>").unwrap();
    let mut timed=Vec::new();
    for line in raw.lines() {
        let Some(c)=line_tag.captures(line) else{continue;};
        let (Ok(start),Ok(length))=(c[1].parse::<u32>(),c[2].parse::<u32>()) else{continue;};
        let Some(end)=start.checked_add(length).filter(|end|*end<=86_400_000) else{continue;};
        let text=word_tag.replace_all(&line[c.get(0).unwrap().end()..],"").trim().to_string();
        timed.push((start,end,text));
    }
    if timed.is_empty(){return parse_lrc(raw);}
    timed.sort_by_key(|line|line.0);
    let mut lines=Vec::new();
    for (i,(start,end,text)) in timed.iter().enumerate() {
        lines.push(LyricsLine{t:*start as f32/1000.0,text:text.clone()});
        if end>start && timed.get(i+1).is_none_or(|next|*end<next.0) {
            lines.push(LyricsLine{t:*end as f32/1000.0,text:String::new()});
        }
    }
    Lyrics{synced:lines.iter().any(|line|!line.text.is_empty()),lines}
}

pub fn fetch(track: &str, artist: &str, duration_secs: f64) -> Result<Lyrics, FetchError> {
    if track.trim().is_empty() || artist.trim().is_empty(){return Err(FetchError::IncompleteMetadata);}
    let path=cache_path(track,artist,duration_secs);
    if let Some(lyrics)=read_cache(&path){return Ok(lyrics);}
    let lyrics=fetch_with_sources("https://music.163.com","https://lrclib.net",track,artist,duration_secs)?;
    if lyrics.synced {write_cache(&path,&lyrics);}
    Ok(lyrics)
}

fn cache_path(track:&str,artist:&str,duration:f64)->std::path::PathBuf {
    let key=format!("{}|{}|{duration:.0}",normalized(track),normalized(artist));
    let hash=key.bytes().fold(14695981039346656037u64,|h,b|(h^b as u64).wrapping_mul(1099511628211));
    crate::model::config_dir().join("lyrics").join(format!("{hash:016x}.lrc"))
}

fn read_cache(path:&std::path::Path)->Option<Lyrics> {
    let lyrics=parse_lrc(&std::fs::read_to_string(path).ok()?);
    (lyrics.synced && lyrics.lines.iter().any(|l|!l.text.is_empty())).then_some(lyrics)
}

fn write_cache(path:&std::path::Path,lyrics:&Lyrics) {
    if let Some(parent)=path.parent(){if std::fs::create_dir_all(parent).is_err(){return;}}
    let raw=lyrics.lines.iter().map(|line|format!("[{:02}:{:06.3}]{}\n",(line.t/60.0) as u32,line.t%60.0,line.text)).collect::<String>();
    let _=crate::model::atomic_write(path,raw.as_bytes());
}

fn fetch_with_sources(netease:&str,lrclib:&str,track:&str,artist:&str,duration:f64)->Result<Lyrics,FetchError> {
    let preferred=fetch_netease(netease,track,artist,duration);
    if preferred.as_ref().is_ok_and(|l|l.synced){return preferred;}
    let fallback=fetch_from(lrclib,track,artist,duration);
    if fallback.as_ref().is_ok_and(|l|l.synced){return fallback;}
    match (preferred,fallback) {
        (Ok(l),_)|(_,Ok(l))=>Ok(l),
        (Err(FetchError::NotFound),Err(e))=>Err(e),
        (Err(e),_)=>Err(e),
    }
}

fn normalized(text:&str)->String {
    text.chars().flat_map(char::to_lowercase).filter(|c|c.is_alphanumeric()).collect()
}

fn netease_candidates(items:&[serde_json::Value],track:&str,artist:&str,duration:f64)->Vec<u64> {
    let title=normalized(track);
    let cleaned=normalized(&clean_title(track));
    let artist_key=normalized(artist);
    let artist_parts:Vec<_>=artist.split(['/', '、', ',', '，', '&', ';', '；']).map(normalized).filter(|s|!s.is_empty()).collect();
    let mut matches=Vec::new();
    for item in items {
        let Some(id)=item.get("id").and_then(|v|v.as_u64()) else{continue;};
        let name=item.get("name").and_then(|v|v.as_str()).unwrap_or_default();
        let exact=normalized(name)==title;
        if !exact && normalized(&clean_title(name))!=cleaned {continue;}
        let artists=item.get("artists").or_else(||item.get("ar")).and_then(|v|v.as_array());
        let Some(artists)=artists else{continue;};
        if !artists.iter().any(|a|a.get("name").and_then(|v|v.as_str()).is_some_and(|a|{
            let a=normalized(a);a==artist_key || artist_parts.contains(&a)
        })){continue;}
        let candidate_duration=item.get("duration").or_else(||item.get("dt")).and_then(|v|v.as_f64()).unwrap_or_default()/1000.0;
        let difference=if duration>0.0 {(candidate_duration-duration).abs()}else{0.0};
        if duration>0.0 && difference>(duration*0.03).max(10.0){continue;}
        matches.push((id,!exact,difference));
    }
    matches.sort_by(|a,b|a.1.cmp(&b.1).then(a.2.total_cmp(&b.2)));
    matches.into_iter().map(|(id,_,_)|id).take(3).collect()
}

fn fetch_netease(base:&str,track:&str,artist:&str,duration:f64)->Result<Lyrics,FetchError> {
    let url=format!("{base}/api/search/get/web?s={}&type=1&limit=12",urlencode(&format!("{track} {artist}")));
    let result=request_json(&url,base)?;
    if result.get("code").and_then(|v|v.as_i64())!=Some(200){return Err(FetchError::Unavailable("网易云搜索未成功".into()));}
    let items=result.pointer("/result/songs").and_then(|v|v.as_array()).cloned().unwrap_or_default();
    let mut plain=None;
    for id in netease_candidates(&items,track,artist,duration) {
        let result=request_json(&format!("{base}/api/song/lyric?id={id}&lv=-1&kv=-1&tv=-1"),base)?;
        if result.get("code").and_then(|v|v.as_i64())!=Some(200){return Err(FetchError::Unavailable("网易云取词未成功".into()));}
        if let Some(raw)=result.pointer("/lrc/lyric").and_then(|v|v.as_str()).filter(|s|!s.trim().is_empty()) {
            let lyrics=parse_lrc(raw);
            if lyrics.synced && lyrics.lines.iter().any(|l|!l.text.is_empty()){return Ok(lyrics);}
            plain=Some(from_plain(raw));
        }
    }
    plain.ok_or(FetchError::NotFound)
}

fn request_json(url:&str,referer:&str)->Result<serde_json::Value,FetchError> {
    let response=ureq::get(url)
        .set("User-Agent",concat!("Daozai/",env!("CARGO_PKG_VERSION")," (Windows)"))
        .set("Referer",referer)
        .set("Accept","application/json")
        .timeout(std::time::Duration::from_secs(5))
        .call().map_err(|e|FetchError::Unavailable(e.to_string()))?;
    let text=response.into_string().map_err(|e|FetchError::InvalidResponse(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e|FetchError::InvalidResponse(e.to_string()))
}

fn clean_title(track:&str)->String {
    let suffix=regex::Regex::new(r"(?i)\s*[\(\[（【]\s*(?:movie\s*ver\.?|official(?:\s+music)?\s+(?:video|audio)|lyrics?|lyric\s+video)\s*[\)\]）】]").unwrap();
    suffix.replace_all(track,"").split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fetch_from(base:&str,track:&str,artist:&str,duration_secs:f64)->Result<Lyrics,FetchError> {
    if track.trim().is_empty() {
        return Err(FetchError::NotFound);
    }
    let cleaned=clean_title(track);
    let mut titles=vec![track.trim()];
    if !cleaned.is_empty() && cleaned!=track.trim(){titles.push(&cleaned);}
    for title in titles {
        let items=search(base,title,artist)?;
        if let Ok(lyrics)=lyrics_from_items(&items,duration_secs){return Ok(lyrics);}
    }
    Err(FetchError::NotFound)
}

fn search(base:&str,track:&str,artist:&str)->Result<Vec<serde_json::Value>,FetchError> {
    let url = format!(
        "{base}/api/search?track_name={}&artist_name={}",
        urlencode(track),
        urlencode(artist)
    );
    let arr=request_json(&url,base)?;
    arr.as_array().cloned().ok_or_else(||FetchError::InvalidResponse("预期歌词列表".into()))
}

fn lyrics_from_items(items:&[serde_json::Value],duration_secs:f64)->Result<Lyrics,FetchError> {
    let mut synced:Option<(Lyrics,f64)>=None;
    let mut plain:Option<(Lyrics,f64)>=None;
    for it in items {
        let d = it.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let score=if duration_secs>0.0{(d-duration_secs).abs()}else{0.0};
        if duration_secs>0.0 && d>0.0 && score>(duration_secs*0.03).max(10.0){continue;}
        if let Some(raw)=it.get("syncedLyrics").and_then(|v|v.as_str()) {
            let parsed=parse_lrc(raw);
            if !parsed.lines.is_empty() && synced.as_ref().is_none_or(|(_,s)|score<*s){synced=Some((parsed,score));}
        }
        if let Some(raw)=it.get("plainLyrics").and_then(|v|v.as_str()).filter(|s|!s.trim().is_empty()) {
            if plain.as_ref().is_none_or(|(_,s)|score<*s){plain=Some((from_plain(raw),score));}
        }
    }
    synced.or(plain).map(|(l,_)|l).ok_or(FetchError::NotFound)
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soda_krc_preserves_real_timing_and_instrumental_gaps() {
        let l=parse_soda("[25378,4523]<0,202,0>孤<202,201,0>单\n[30938,5563]<0,202,0>身<202,201,0>边");
        assert!(l.synced);
        assert!((l.lines[0].t-25.378).abs()<0.001);
        assert_eq!(l.lines[0].text,"孤单");
        assert_eq!(l.lines[l.index_at(30.0).unwrap()].text,"");
        assert_eq!(l.lines[l.index_at(31.0).unwrap()].text,"身边");
        assert!(!parse_soda("[bad,10]<0,10,0>错").synced);
        assert!(!parse_soda("纯文本歌词").synced);
        assert_eq!(parse_soda("[00:01.20]正文").lines[0].text,"正文");
    }


    #[test]
    #[ignore="需要实际访问外部歌词服务"]
    fn live_reference_track() {
        let result=fetch("Never Gonna Give You Up","Rick Astley",213.0);
        println!("reference: {:?}",result.as_ref().map(|l|(l.synced,l.lines.len())));
        assert!(result.is_ok());
    }

    #[test]
    #[ignore="需要实际访问外部歌词服务"]
    fn live_web_player_track() {
        let result=fetch("スパークル (movie ver.)","RADWIMPS",534.0);
        println!("web-player: {:?}",result.as_ref().map(|l|(l.synced,l.lines.len())));
        assert!(result.unwrap().synced);
    }

    #[test]
    #[ignore="需要实际访问外部歌词服务"]
    fn live_chinese_tracks() {
        for (title,artist,duration) in [("稀客","杨千嬅",190.0),("阿拉斯加海湾","蓝心羽",241.0)] {
            let lyrics=fetch_with_sources("https://music.163.com","https://lrclib.net",title,artist,duration).unwrap();
            assert!(lyrics.synced);
            let first=lyrics.index_at(30.0).unwrap();let later=lyrics.index_at(90.0).unwrap();
            println!("{title} / {artist}: {} 条时间戳，30 秒行号 {first}，90 秒行号 {later}",lyrics.lines.len());
            assert!(later>first,"歌词应随播放进度前进");
        }
    }

    #[test]
    fn matching_rejects_covers_wrong_titles_and_other_versions() {
        let items=serde_json::json!([
            {"id":1,"name":"Track","artists":[{"name":"Cover Artist"}],"duration":120000},
            {"id":2,"name":"Different Track","artists":[{"name":"Artist"}],"duration":120000},
            {"id":3,"name":"Track (Live)","artists":[{"name":"Artist"}],"duration":120000},
            {"id":4,"name":"Track","artists":[{"name":"Artist"}],"duration":200000},
            {"id":5,"name":"Track","artists":[{"name":"Artist"}],"duration":121000},
            {"id":6,"name":"Track","ar":[{"name":"Artist"}],"dt":122000}
        ]);
        assert_eq!(netease_candidates(items.as_array().unwrap(),"Track","Artist",120.0),vec![5,6]);
    }

    fn serve(responses:Vec<(&'static str,String)>)->(String,std::thread::JoinHandle<()>) {
        use std::{io::{Read,Write},net::TcpListener};
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();
        let base=format!("http://{}",listener.local_addr().unwrap());
        let worker=std::thread::spawn(move||for (status,body) in responses {
            let (mut stream,_)=listener.accept().unwrap();
            let mut buf=[0;4096];let _=stream.read(&mut buf).unwrap();
            write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        });
        (base,worker)
    }

    #[test]
    fn online_resolver_uses_a_working_source_when_the_other_is_down() {
        let search=serde_json::json!({"code":200,"result":{"songs":[{"id":5,"name":"Track","artists":[{"name":"Artist"}],"duration":120000}]}}).to_string();
        let lyric=serde_json::json!({"code":200,"lrc":{"lyric":"[00:01.00]first\n[00:05.00]second"}}).to_string();
        let (netease,worker)=serve(vec![("200 OK",search),("200 OK",lyric)]);
        // 不可达的备用地址不应影响主源正常返回。
        assert!(fetch_with_sources(&netease,"http://127.0.0.1:1","Track","Artist",120.0).unwrap().synced);
        worker.join().unwrap();
        let (netease,first)=serve(vec![("503 Service Unavailable",String::new())]);
        let (lrclib,second)=serve(vec![("200 OK",r#"[{"duration":120,"syncedLyrics":"[00:01.00]first\n[00:05.00]second"}]"#.into())]);
        assert!(fetch_with_sources(&netease,&lrclib,"Track","Artist",120.0).unwrap().synced);
        first.join().unwrap();second.join().unwrap();
    }

    #[test]
    fn cached_lyrics_preserve_timing_and_invalid_caches_are_ignored() {
        let path=std::env::temp_dir().join(format!("daozai-lrc-cache-{}-{:?}.lrc",std::process::id(),std::thread::current().id()));
        let expected=parse_lrc("[00:01.234]first\n[00:12.567]second\n[01:03.891]third");
        write_cache(&path,&expected);
        let cached=read_cache(&path).unwrap();
        assert_eq!(cached.lines.len(),expected.lines.len());
        for (a,b) in cached.lines.iter().zip(&expected.lines) {
            assert_eq!(a.text,b.text);assert!((a.t-b.t).abs()<0.002);
        }
        std::fs::write(&path,"broken cache").unwrap();
        assert!(read_cache(&path).is_none());
        let _=std::fs::remove_file(path);
    }

    #[test]
    fn parser_keeps_fractional_colon_times_and_rejects_invalid_timing() {
        let lyrics=parse_lrc("[00:12:35]first\n[00:NaN]invalid\n[00:61.00]invalid\n[00:12:35:10]invalid");
        assert_eq!(lyrics.lines.len(),1);assert!((lyrics.lines[0].t-12.35).abs()<0.001);
    }

    #[test]
    fn parses_and_sorts_lrc() {
        let raw = "[00:12.50]second\n[00:01.00]first\n[00:20:00]third";
        let l = parse_lrc(raw);
        assert_eq!(l.lines.len(), 3);
        assert_eq!(l.lines[0].text, "first");
        assert_eq!(l.lines[1].text, "second");
        assert!((l.lines[2].t - 20.0).abs() < 1e-3);
        assert!(l.synced);
    }

    #[test]
    fn index_at_tracks_position() {
        let l = parse_lrc("[00:01.00]a\n[00:05.00]b\n[00:09.00]c");
        assert_eq!(l.index_at(0.0), None);
        assert_eq!(l.index_at(6.0), Some(1));
        assert_eq!(l.index_at(100.0), Some(2));
    }

    #[test]
    fn plain_lyrics_have_no_invented_timing() {
        let l = from_plain("one\ntwo\n\nthree");
        assert_eq!(l.lines.len(), 3);
        assert!(!l.synced);
        assert!(l.lines.iter().all(|line|line.t==0.0));
    }

    #[test]
    fn title_fallback_keeps_meaningful_versions() {
        assert_eq!(clean_title("スパークル (movie ver.)"),"スパークル");
        assert_eq!(clean_title("Track (Official Music Video)"),"Track");
        assert_eq!(clean_title("Track (Live)"),"Track (Live)");
    }

    #[test]
    fn matching_prefers_synced_lyrics_and_rejects_other_versions() {
        let items=serde_json::json!([
            {"duration":534,"plainLyrics":"plain"},
            {"duration":535,"syncedLyrics":"[00:01.00]first\n[00:05.00]second"},
            {"duration":200,"syncedLyrics":"[00:01.00]wrong version"}
        ]);
        let result=lyrics_from_items(items.as_array().unwrap(),534.0).unwrap();
        assert!(result.synced);assert_eq!(result.lines[0].text,"first");
        assert_eq!(lyrics_from_items(&[items[2].clone()],534.0).unwrap_err(),FetchError::NotFound);
    }

    #[test]
    fn http_search_and_network_errors_are_distinct_from_missing_lyrics() {
        use std::{io::{Read,Write},net::TcpListener};
        for (status,body,expected) in [("200 OK",r#"[{"duration":120,"syncedLyrics":"[00:01.00]first"}]"#,0),("200 OK","[]",1),("502 Bad Gateway","",2)] {
            let listener=TcpListener::bind("127.0.0.1:0").unwrap();
            let base=format!("http://{}",listener.local_addr().unwrap());
            let worker=std::thread::spawn(move||{
                let (mut stream,_)=listener.accept().unwrap();
                let mut buf=[0;4096];let n=stream.read(&mut buf).unwrap();
                let request=String::from_utf8_lossy(&buf[..n]);
                assert!(request.contains("track_name=Track%20Name&artist_name=Artist"));
                write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            });
            let result=fetch_from(&base,"Track Name","Artist",120.0);
            worker.join().unwrap();
            match expected {0=>assert!(result.unwrap().synced),1=>assert_eq!(result.unwrap_err(),FetchError::NotFound),_=>assert!(matches!(result,Err(FetchError::Unavailable(_))))}
        }
    }

    #[test]
    fn urlencode_escapes_spaces_and_symbols() {
        assert_eq!(urlencode("a b&c"), "a%20b%26c");
    }
}

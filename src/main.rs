//! waku — ani-cli style CLI for anime-waku.com (Thai-sub anime)
//!
//! search -> pick anime (shows episode count) -> pick episode -> pick player
//! -> mpv -> next / previous / replay menu
//!
//! Search strategy (the site's built-in search is noisy, so we combine):
//!   1. a local index of every /anime/ page (from the site's sitemap, or by
//!      crawling the /anime/ archive), ranked offline with word-level fuzzy
//!      matching (handles "re zero" / "re:zero" / "rezero", typos, Thai text)
//!   2. the site's own `/?s=` search (anime, episode and tag hits are all
//!      mapped back to the anime page)
//! Every candidate page is then opened to get the real title + episode count.

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use dialoguer::{theme::ColorfulTheme, FuzzySelect, Input, Select};
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT_LANGUAGE, REFERER, USER_AGENT};
use scraper::{ElementRef, Html, Selector};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};
use url::Url;

/// Direct media: `*.m3u8` / `*.mp4`, or an HLS playlist served as
/// `/m3u8/<id>.txt` (doodee-player disguises playlists and segments).
const MEDIA_PAT: &str = r#"https?://[^"'\s<>\\]+?(?:\.(?:m3u8|mp4)|/m3u8/[^"'\s<>\\?]+\.txt)[^"'\s<>\\]*"#;

fn is_media_path(path: &str) -> bool {
    let p = path.to_lowercase();
    let p = p.split('?').next().unwrap_or("");
    p.ends_with(".m3u8") || p.ends_with(".mp4") || (p.contains("/m3u8/") && p.ends_with(".txt"))
}

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// Site root. Override with WAKU_BASE (handy for testing / mirrors).
fn base() -> &'static str {
    static B: OnceLock<String> = OnceLock::new();
    B.get_or_init(|| {
        std::env::var("WAKU_BASE")
            .unwrap_or_else(|_| "https://anime-waku.com".into())
            .trim_end_matches('/')
            .to_string()
    })
}

#[derive(Parser)]
#[command(name = "waku", version, about = "ani-cli style player for anime-waku.com")]
struct Cli {
    /// Search query (omit to be prompted)
    query: Vec<String>,

    /// Open an anime page URL directly (skip search)
    #[arg(short, long)]
    url: Option<String>,

    /// Episode number to play directly (e.g. 3)
    #[arg(short, long)]
    episode: Option<f32>,

    /// Player/server index to use (1-based). Default: ask
    #[arg(short, long)]
    player: Option<usize>,

    /// Only print the resolved stream/embed URL, don't launch a player
    #[arg(long)]
    print: bool,

    /// Print ranked search results (score, episodes, title, url) and exit
    #[arg(long)]
    list: bool,

    /// Rebuild the local anime index now
    #[arg(long)]
    reindex: bool,

    /// Debug an episode page URL: saves its HTML and tries every player endpoint
    #[arg(long, value_name = "EP_URL")]
    debug_ep: Option<String>,

    /// Show the server menu instead of auto-picking the best working one
    #[arg(long)]
    choose_server: bool,

    /// Video quality: best, worst, or a height such as 1080 / 720 / 480 / 360
    /// (the closest available is used; your last choice is remembered)
    #[arg(short, long)]
    quality: Option<String>,

    /// Show mpv/ffmpeg demuxer warnings (hidden by default)
    #[arg(long)]
    show_warnings: bool,

    /// Player command
    #[arg(long, default_value = "mpv")]
    player_cmd: String,
}

#[derive(Debug, Clone)]
struct Episode {
    season: u32,
    num: f32,
    label: String,
    url: String,
}

#[derive(Debug, Clone)]
struct Anime {
    url: String,
    title: String,
    eps: Vec<Episode>,
    tags: Vec<String>,
    score: f32,
}

#[derive(Debug, Clone)]
struct PlayerOpt {
    name: String,
    kind: PlayerKind,
}

#[derive(Debug, Clone)]
enum PlayerKind {
    Direct(String),
    Api { post: String, typ: String, nume: String },
}

// ---------------------------------------------------------------- http utils

fn client() -> Result<Client> {
    let mut h = HeaderMap::new();
    h.insert(USER_AGENT, HeaderValue::from_static(UA));
    h.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("th,en;q=0.8"));
    Ok(Client::builder()
        .default_headers(h)
        .timeout(Duration::from_secs(25))
        .build()?)
}

fn get(c: &Client, url: &str, referer: Option<&str>) -> Result<String> {
    let mut req = c.get(url);
    if let Some(r) = referer {
        req = req.header(REFERER, r);
    }
    let resp = req.send().with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url} -> HTTP {}", resp.status());
    }
    Ok(resp.text()?)
}

fn abs(base_url: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.starts_with("//") {
        return Some(format!("https:{href}"));
    }
    Url::parse(base_url).ok()?.join(href).ok().map(|u| u.to_string())
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("bad selector")
}

fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn path_segs(url: &str) -> Vec<String> {
    Url::parse(url)
        .ok()
        .and_then(|u| u.path_segments().map(|s| s.filter(|x| !x.is_empty()).map(String::from).collect()))
        .unwrap_or_default()
}

/// Run `f` over `items` with up to 8 threads at a time (order preserved).
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let mut out = Vec::with_capacity(items.len());
    for chunk in items.chunks(8) {
        let res: Vec<Option<R>> = std::thread::scope(|s| {
            let f = &f;
            let hs: Vec<_> = chunk.iter().map(|it| s.spawn(move || f(it))).collect();
            hs.into_iter().map(|h| h.join().ok()).collect()
        });
        out.extend(res.into_iter().flatten());
    }
    out
}

// ------------------------------------------------------------------ ranking

fn norm(s: &str) -> String {
    let t: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    clean(&t)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Similarity of one query word to one title word (0.0 ..= 1.0).
fn word_sim(q: &str, h: &str) -> f32 {
    if q == h {
        return 1.0;
    }
    if !q.is_ascii() {
        // Thai etc. has no word breaks: plain substring match
        return if h.contains(q) { 0.9 } else { 0.0 };
    }
    let ql = q.chars().count();
    let hl = h.chars().count();
    if ql >= 3 && h.starts_with(q) {
        return 0.65;
    }
    if ql == 2 && h.starts_with(q) {
        return 0.3;
    }
    let maxd = if ql >= 8 { 2 } else if ql >= 4 { 1 } else { 0 };
    if maxd > 0 && hl.abs_diff(ql) <= maxd {
        let d = levenshtein(q, h);
        if d <= maxd {
            return 0.85 - 0.1 * (d as f32 - 1.0).max(0.0);
        }
    }
    0.0
}

/// 0.0 ..= 1.0 — how well `hay` matches `query`.
fn score(query: &str, hay: &str) -> f32 {
    let q = norm(query);
    let h = norm(hay);
    let qw: Vec<&str> = q.split(' ').filter(|x| !x.is_empty()).collect();
    let hw: Vec<&str> = h.split(' ').filter(|x| !x.is_empty()).collect();
    if qw.is_empty() || hw.is_empty() {
        return 0.0;
    }
    let sum: f32 = qw
        .iter()
        .map(|t| hw.iter().map(|w| word_sim(t, w)).fold(0.0, f32::max))
        .sum();
    let mut s = sum / qw.len() as f32;

    // whole phrase present as separate words => strong signal
    if format!(" {h} ").contains(&format!(" {q} ")) {
        s += 0.25;
    } else if qw.len() > 1 {
        // words present in the same order
        let mut pos = 0;
        let mut ok = true;
        for t in &qw {
            match hw[pos..].iter().position(|w| word_sim(t, w) >= 0.65) {
                Some(i) => pos += i + 1,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            s += 0.1;
        }
    }
    // "rezero" <-> "re zero" <-> "re:zero"
    let qc = qw.join("");
    let hc = hw.join("");
    if qc.chars().count() >= 4 && hc.contains(&qc) {
        s = s.max(0.9);
    }
    s.min(1.0)
}

// -------------------------------------------------------------- link parsing

/// Collect `/anime/<slug>/` links (series pages) from any HTML, de-duplicated.
fn anime_links(html: &str, base_url: &str) -> Vec<(String, String)> {
    let doc = Html::parse_document(html);
    let img = sel("img");
    let mut out: Vec<(String, String)> = Vec::new();
    for el in doc.select(&sel("a[href]")) {
        let Some(full) = el.value().attr("href").and_then(|h| abs(base_url, h)) else { continue };
        let segs = path_segs(&full);
        if segs.len() != 2 || segs[0] != "anime" {
            continue;
        }
        let mut title = clean(&el.text().collect::<String>());
        if title.is_empty() {
            title = el.value().attr("title").map(clean).unwrap_or_default();
        }
        if title.is_empty() {
            if let Some(i) = el.select(&img).next() {
                title = i.value().attr("alt").map(clean).unwrap_or_default();
            }
        }
        match out.iter_mut().find(|i| i.0 == full) {
            Some(it) => {
                if title.len() > it.1.len() {
                    it.1 = title;
                }
            }
            None => out.push((full, title)),
        }
    }
    out
}

fn slug_words(url: &str) -> String {
    path_segs(url).get(1).cloned().unwrap_or_default().replace('-', " ")
}

// -------------------------------------------------------------- local index

fn cache_file() -> PathBuf {
    let root = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    root.join("waku").join("index.tsv")
}

fn index_from_sitemaps(c: &Client) -> Vec<(String, String)> {
    let roots = [
        "/wp-sitemap.xml",
        "/sitemap_index.xml",
        "/sitemap.xml",
        "/sitemap-index.xml",
        "/anime-sitemap.xml",
        "/wp-sitemap-posts-anime-1.xml",
    ];
    let loc = Regex::new(r"<loc>\s*([^<\s]+)\s*</loc>").unwrap();
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut seen_xml: HashSet<String> = HashSet::new();
    let mut other_children: Vec<String> = Vec::new();
    let mut queue: Vec<String> = roots.iter().rev().map(|r| format!("{}{}", base(), r)).collect();

    let mut process = |queue: &mut Vec<String>, entries: &mut Vec<(String, String)>, filter: bool, others: &mut Vec<String>| {
        while let Some(u) = queue.pop() {
            if seen_xml.len() > 80 || !seen_xml.insert(u.clone()) {
                continue;
            }
            let Ok(xml) = get(c, &u, None) else { continue };
            for cap in loc.captures_iter(&xml) {
                let l = cap[1].replace("&amp;", "&");
                if l.contains(".xml") {
                    let low = l.to_lowercase();
                    let wanted = ["anime", "tvshow", "series"].iter().any(|k| low.contains(k));
                    if !filter || wanted {
                        queue.push(l);
                    } else {
                        others.push(l);
                    }
                } else {
                    let segs = path_segs(&l);
                    if segs.len() == 2 && segs[0] == "anime" && !entries.iter().any(|e| e.0 == l) {
                        let title = segs[1].replace('-', " ");
                        entries.push((l, title));
                    }
                }
            }
        }
    };
    process(&mut queue, &mut entries, true, &mut other_children);
    if entries.is_empty() {
        // sitemap names did not mention "anime": try the remaining children
        let mut rest: Vec<String> = other_children
            .iter()
            .filter(|u| {
                let low = u.to_lowercase();
                !["episode", "-ep-", "/ep", "movie"].iter().any(|k| low.contains(k))
            })
            .take(30)
            .cloned()
            .collect();
        let mut dummy = Vec::new();
        process(&mut rest, &mut entries, false, &mut dummy);
    }
    entries
}

fn index_from_crawl(c: &Client) -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = Vec::new();
    let mut page = 1usize;
    while page <= 250 {
        let pages: Vec<usize> = (page..page + 6).collect();
        let res: Vec<Vec<(String, String)>> = par_map(&pages, |p| {
            let u = if *p == 1 { format!("{}/anime/", base()) } else { format!("{}/anime/page/{p}/", base()) };
            get(c, &u, Some(base())).map(|h| anime_links(&h, &u)).unwrap_or_default()
        });
        let before = all.len();
        for list in res {
            for e in list {
                if !all.iter().any(|a| a.0 == e.0) {
                    all.push(e);
                }
            }
        }
        eprint!("\r  crawled {} anime so far...", all.len());
        if all.len() == before {
            break;
        }
        page += 6;
    }
    eprintln!();
    all
}

fn load_index(c: &Client, force: bool) -> Vec<(String, String)> {
    let path = cache_file();
    let read = |p: &PathBuf| -> Vec<(String, String)> {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('\t').map(|(u, t)| (u.to_string(), t.to_string())))
            .collect()
    };
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|age| age < Duration::from_secs(12 * 3600))
        .unwrap_or(false);
    if fresh && !force {
        let idx = read(&path);
        if !idx.is_empty() {
            return idx;
        }
    }
    eprintln!("Building local anime index (one-time, cached for 12h)...");
    let mut idx = index_from_sitemaps(c);
    if idx.is_empty() {
        idx = index_from_crawl(c);
    }
    if idx.is_empty() {
        return read(&path); // stale cache is better than nothing
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body: String = idx
        .iter()
        .map(|(u, t)| format!("{}\t{}\n", u, t.replace(['\t', '\n', '\r'], " ")))
        .collect();
    let _ = std::fs::write(&path, body);
    eprintln!("Indexed {} anime.", idx.len());
    idx
}

// ------------------------------------------------------------- site search

/// Use the site's own search. Hits may be anime pages, episode pages or tag
/// pages — all are mapped back to an anime page.
fn remote_search(c: &Client, q: &str) -> Vec<(String, String)> {
    let mut variants = vec![q.to_string()];
    let compact = norm(q).replace(' ', "");
    if norm(q).contains(' ') && compact.chars().count() >= 4 {
        variants.push(compact);
    }
    let mut direct: Vec<(String, String)> = Vec::new();
    let mut eps: Vec<String> = Vec::new();

    for (vi, v) in variants.iter().enumerate() {
        let max_pages = if vi == 0 { 3 } else { 1 };
        for page in 1..=max_pages {
            let b = if page == 1 { format!("{}/", base()) } else { format!("{}/page/{page}/", base()) };
            let Ok(url) = Url::parse_with_params(&b, &[("s", v.as_str())]) else { break };
            let Ok(html) = get(c, url.as_str(), Some(base())) else { break };
            let doc = Html::parse_document(&html);
            let before = direct.len() + eps.len();
            for el in doc.select(&sel("a[href]")) {
                let Some(full) = el.value().attr("href").and_then(|h| abs(url.as_str(), h)) else { continue };
                let segs = path_segs(&full);
                if segs.len() != 2 {
                    continue;
                }
                let text = clean(&el.text().collect::<String>());
                match segs[0].as_str() {
                    "anime" => {
                        if !direct.iter().any(|d| d.0 == full) {
                            direct.push((full, text));
                        }
                    }
                    // tag slug usually equals the anime slug
                    "tag" => {
                        let u = format!("{}/anime/{}/", base(), segs[1]);
                        if !direct.iter().any(|d| d.0 == u) {
                            direct.push((u, text));
                        }
                    }
                    "ep" => {
                        if !eps.contains(&full) {
                            eps.push(full);
                        }
                    }
                    _ => {}
                }
            }
            if direct.len() + eps.len() == before {
                break;
            }
        }
    }

    eps.truncate(8);
    let resolved: Vec<Option<(String, String)>> = par_map(&eps, |e| {
        let html = get(c, e, Some(base())).ok()?;
        anime_links(&html, e).into_iter().next()
    });
    for r in resolved.into_iter().flatten() {
        if !direct.iter().any(|d| d.0 == r.0) {
            direct.push(r);
        }
    }
    direct
}

fn find_anime(c: &Client, q: &str, index: &[(String, String)]) -> Vec<Anime> {
    let mut cands: Vec<(String, String)> = Vec::new();

    // 1) offline fuzzy ranking over the local index
    let mut local: Vec<(f32, &(String, String))> = index
        .iter()
        .map(|e| (score(q, &format!("{} {}", e.1, slug_words(&e.0))), e))
        .filter(|x| x.0 >= 0.5)
        .collect();
    local.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1 .0.len().cmp(&b.1 .0.len()))
    });
    for (_, e) in local.iter().take(14) {
        cands.push((e.0.clone(), e.1.clone()));
    }

    // 2) the site's own search
    for r in remote_search(c, q) {
        if cands.len() >= 26 {
            break;
        }
        if !cands.iter().any(|a| a.0 == r.0) {
            cands.push(r);
        }
    }

    // 3) open each candidate: real title + real episode count
    let loaded: Vec<Option<Anime>> = par_map(&cands, |(u, h)| load_anime(c, u, h).ok());
    let mut out: Vec<Anime> = loaded.into_iter().flatten().collect();

    for a in out.iter_mut() {
        a.score = score(q, &format!("{} {}", a.title, slug_words(&a.url)));
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.eps.len().cmp(&a.eps.len()))
            .then(a.title.chars().count().cmp(&b.title.chars().count()))
            .then(a.title.cmp(&b.title))
    });
    let top = out.first().map(|a| a.score).unwrap_or(0.0);
    let cutoff = (top - 0.35).max(0.35);
    out.retain(|a| a.score >= cutoff);
    out
}

// -------------------------------------------------------------- anime page

fn fmt_num(n: f32) -> String {
    if n.fract() == 0.0 { format!("{}", n as u32) } else { format!("{n}") }
}

fn load_anime(c: &Client, url: &str, hint: &str) -> Result<Anime> {
    let html = get(c, url, Some(base()))?;
    let doc = Html::parse_document(&html);

    let h1 = doc
        .select(&sel("h1"))
        .next()
        .map(|h| clean(&h.text().collect::<String>()))
        .unwrap_or_else(|| hint.to_string());

    // h1 looks like: "<Romaji> <Thai title> ตอนที่ 1-2 ซับไทย ยังไม่จบ"
    let mut tags = Vec::new();
    for (th, en) in [("ซับไทย", "Thai sub"), ("พากย์ไทย", "Thai dub"), ("ยังไม่จบ", "Ongoing"), ("จบแล้ว", "Completed")] {
        if h1.contains(th) {
            tags.push(en.to_string());
        }
    }
    let cut = Regex::new(r"\s*(ตอนที่|ซับไทย|พากย์ไทย|ยังไม่จบ|จบแล้ว).*$")?;
    let mut title = cut.replace(&h1, "").to_string();
    if title.is_empty() {
        title = if hint.is_empty() { slug_words(url) } else { hint.to_string() };
    }

    let ep_sel = sel(r#"a[href*="/ep/"]"#);
    let se_re = Regex::new(r"^\s*(\d+)\s*-\s*(\d+(?:\.\d+)?)\b")?;
    let th_re = Regex::new(r"ตอนที่\s*(\d+(?:\.\d+)?)")?;
    let mut seen = HashSet::new();
    let mut eps: Vec<Episode> = Vec::new();

    for el in doc.select(&ep_sel) {
        let Some(full) = el.value().attr("href").and_then(|h| abs(url, h)) else { continue };
        let segs = path_segs(&full);
        if segs.first().map(String::as_str) != Some("ep") || !seen.insert(full.clone()) {
            continue;
        }
        let link_text = clean(&el.text().collect::<String>());

        // "S - E" label in the nearest ancestor that holds only this link
        let mut season = 1u32;
        let mut num: Option<f32> = None;
        for anc in el.ancestors().take(4) {
            let Some(a) = ElementRef::wrap(anc) else { continue };
            if a.select(&ep_sel).count() != 1 {
                break;
            }
            let t = clean(&a.text().collect::<String>());
            if let Some(m) = se_re.captures(&t) {
                season = m[1].parse().unwrap_or(1);
                num = m[2].parse().ok();
                break;
            }
        }
        if num.is_none() {
            num = th_re.captures(&link_text).and_then(|m| m[1].parse().ok());
        }
        eps.push(Episode { season: season.max(1), num: num.unwrap_or(0.0), label: link_text, url: full });
    }
    let id = |u: &str| path_segs(u).get(1).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    eps.sort_by(|a, b| {
        (a.season, a.num)
            .partial_cmp(&(b.season, b.num))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(id(&a.url).cmp(&id(&b.url)))
    });
    if eps.iter().all(|e| e.num == 0.0) {
        for (i, e) in eps.iter_mut().enumerate() {
            e.num = (i + 1) as f32;
        }
    }
    let multi = eps.iter().any(|e| e.season != eps[0].season);
    for e in eps.iter_mut() {
        if e.num > 0.0 {
            e.label = if multi {
                format!("S{} Episode {}", e.season, fmt_num(e.num))
            } else {
                format!("Episode {}", fmt_num(e.num))
            };
        }
    }
    Ok(Anime { url: url.to_string(), title, eps, tags, score: 0.0 })
}

// --------------------------------------------------------------- players

fn players(c: &Client, ep_url: &str) -> Result<Vec<PlayerOpt>> {
    let html = get(c, ep_url, Some(base()))?;
    let doc = Html::parse_document(&html);
    let mut out = Vec::new();

    // DooPlay: <li class="dooplay_player_option" data-post data-type data-nume>
    for li in doc.select(&sel("li.dooplay_player_option, #playeroptionsul li")) {
        let v = li.value();
        let (Some(post), Some(nume)) = (v.attr("data-post"), v.attr("data-nume")) else { continue };
        let name = clean(&li.text().collect::<String>());
        out.push(PlayerOpt {
            name: if name.is_empty() { format!("player {nume}") } else { name },
            kind: PlayerKind::Api {
                post: post.into(),
                typ: v.attr("data-type").unwrap_or("tv").into(),
                nume: nume.into(),
            },
        });
    }

    // Fallback: any iframe (src / data-src / data-lazy-src)
    for f in doc.select(&sel("iframe")) {
        let v = f.value();
        let src = v
            .attr("src")
            .filter(|s| !s.is_empty() && !s.starts_with("about:"))
            .or_else(|| v.attr("data-src"))
            .or_else(|| v.attr("data-lazy-src"));
        if let Some(s) = src.and_then(|s| abs(ep_url, s)) {
            if s.contains("googletagmanager") {
                continue;
            }
            out.push(PlayerOpt { name: format!("iframe {}", out.len() + 1), kind: PlayerKind::Direct(s) });
        }
    }

    // iframes written inside inline scripts / JSON (escaped quotes)
    if let Ok(re) = Regex::new(r#"(?i)<iframe[^>]+?src=\\?["']([^"'\\]+)"#) {
        for cap in re.captures_iter(&html) {
            let u = cap[1].replace("\\/", "/");
            if let Some(full) = abs(ep_url, &u) {
                let known = out.iter().any(|o| matches!(&o.kind, PlayerKind::Direct(d) if *d == full));
                if !known && !full.contains("googletagmanager") {
                    out.push(PlayerOpt { name: format!("embed {}", out.len() + 1), kind: PlayerKind::Direct(full) });
                }
            }
        }
    }

    // Last resort: embed URLs hidden in inline scripts / JSON
    if out.is_empty() {
        let re = Regex::new(MEDIA_PAT)?;
        let unescaped = html.replace("\\/", "/").replace("&amp;", "&");
        for m in re.find_iter(&unescaped) {
            let u = m.as_str().to_string();
            out.push(PlayerOpt { name: format!("stream {}", out.len() + 1), kind: PlayerKind::Direct(u) });
        }
    }

    if out.is_empty() {
        bail!("no player found on {ep_url} (site markup may differ — see README notes)");
    }
    Ok(out)
}

/// Pull an embed / stream URL out of a player API response (JSON, an
/// <iframe> snippet, or a bare URL).
fn extract_embed(body: &str, page: &str) -> Option<String> {
    let t = body.trim();
    let mut text = t.to_string();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
        for k in ["embed_url", "url", "file", "src", "embed", "iframe", "link"] {
            if let Some(x) = v.get(k).and_then(|x| x.as_str()) {
                text = x.to_string();
                break;
            }
        }
    }
    let text = text.replace("\\/", "/").replace("&amp;", "&");
    let iframe = Regex::new(r#"(?i)<iframe[^>]+src=["']([^"']+)["']"#).ok()?;
    if let Some(c) = iframe.captures(&text) {
        return abs(page, &c[1]);
    }
    let one = text.trim();
    if (one.starts_with("http") || one.starts_with("//")) && !one.contains(char::is_whitespace) && !one.contains('<') {
        return abs(page, one);
    }
    let media = Regex::new(MEDIA_PAT).ok()?;
    media.find(&text).map(|m| m.as_str().to_string())
}

fn resolve(c: &Client, p: &PlayerOpt, ep_url: &str) -> Result<String> {
    match &p.kind {
        PlayerKind::Direct(u) => Ok(u.clone()),
        PlayerKind::Api { post, typ, nume } => {
            let b = base();
            let mut errs: Vec<String> = Vec::new();
            // DooPlay REST v2, then v1
            let urls = [
                format!("{b}/wp-json/dooplayer/v2/{post}/{typ}/{nume}"),
                format!("{b}/wp-json/dooplayer/v1/post/{post}?type={typ}&source={nume}"),
            ];
            for u in urls {
                match get(c, &u, Some(ep_url)) {
                    Ok(body) => match extract_embed(&body, ep_url) {
                        Some(e) => return Ok(e),
                        None => errs.push(format!("{u}: response had no embed ({})", truncate(&body, 100))),
                    },
                    Err(e) => errs.push(format!("{e:#}")),
                }
            }
            // older DooPlay: admin-ajax
            let ajax = format!("{b}/wp-admin/admin-ajax.php");
            let r = c
                .post(&ajax)
                .header(REFERER, ep_url)
                .header("X-Requested-With", "XMLHttpRequest")
                .form(&[("action", "doo_player_ajax"), ("post", post.as_str()), ("nume", nume.as_str()), ("type", typ.as_str())])
                .send();
            match r {
                Ok(resp) => {
                    let st = resp.status();
                    let body = resp.text().unwrap_or_default();
                    if st.is_success() {
                        if let Some(e) = extract_embed(&body, ep_url) {
                            return Ok(e);
                        }
                    }
                    errs.push(format!("POST {ajax} -> HTTP {st}: {}", truncate(&body, 100)));
                }
                Err(e) => errs.push(format!("POST {ajax}: {e}")),
            }
            Err(anyhow!("could not resolve player (post {post}, nume {nume}):\n  {}", errs.join("\n  ")))
        }
    }
}

/// If the embed is a host page, try to dig a direct m3u8/mp4 out of it.
/// Hosts that mpv's yt-dlp hook already understands: hand the page URL over as-is.
fn ytdlp_host(url: &str) -> bool {
    let u = url.to_lowercase();
    ["ok.ru", "youtube.com", "youtu.be", "dailymotion.com", "vimeo.com", "streamable.com", "drive.google.com"]
        .iter()
        .any(|h| u.contains(h))
}

static VERBOSE: AtomicBool = AtomicBool::new(false);
static HOP_N: AtomicUsize = AtomicUsize::new(0);

fn vlog(msg: impl AsRef<str>) {
    if VERBOSE.load(Ordering::Relaxed) {
        eprintln!("{}", msg.as_ref());
    }
}

/// `https://host[:port]/` — what a browser sends as Referer across origins.
fn origin_of(url: &str) -> String {
    match Url::parse(url) {
        Ok(u) => match (u.host_str(), u.port()) {
            (Some(h), Some(p)) => format!("{}://{h}:{p}/", u.scheme()),
            (Some(h), None) => format!("{}://{h}/", u.scheme()),
            _ => url.to_string(),
        },
        Err(_) => url.to_string(),
    }
}

/// Worth following while hunting for a stream? (skips images, scripts, trackers)
fn followable(u: &str) -> bool {
    let low = u.to_lowercase();
    let path = low.split(['?', '#']).next().unwrap_or("");
    let assets: &[&str] = &[".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".ico", ".css", ".js", ".woff", ".woff2", ".ttf", ".vtt", ".srt"];
    let junk: &[&str] = &["googletagmanager", "google-analytics", "gstatic.com", "googleapis.com", "facebook.", "histats", "doubleclick", "/cdn-cgi/"];
    !assets.iter().any(|e| path.ends_with(e)) && !junk.iter().any(|j| low.contains(j))
}

/// Minimal base64 decoder (for `atob("...")` blobs); no extra crate needed.
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for ch in s.bytes() {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Other pages worth opening from this one: iframes, plus JSON/JS fields such as
/// `"src": "https://…"` (the site's server list), `file: '…'`, `embed_url` …
fn candidates(txt: &str, page: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |raw: &str| {
        if let Some(full) = abs(page, raw) {
            if followable(&full) && full != page && !out.contains(&full) {
                out.push(full);
            }
        }
    };
    if let Ok(re) = Regex::new(r#"(?i)<iframe[^>]+?\ssrc=["']([^"']+)["']"#) {
        for c in re.captures_iter(txt) {
            push(&c[1]);
        }
    }
    if let Ok(re) = Regex::new(r#"(?i)["']?(?:src|file|url|embed|embed_url|iframe|link|source|player)["']?\s*[:=]\s*["'](https?://[^"'\s<>]+)["']"#) {
        for c in re.captures_iter(txt) {
            push(&c[1]);
        }
    }
    out
}

/// Look for a direct stream (.m3u8 / .mp4 / `/m3u8/*.txt`) behind an embed,
/// following nested iframes and JSON `src` links (depth-limited, loop-safe).
/// Returns (stream_url, page_it_was_found_on); the page doubles as Referer.
fn sniff_stream(c: &Client, url: &str, referer: &str) -> Option<(Vec<String>, String)> {
    let mut seen = HashSet::new();
    sniff(c, url, referer, 5, &mut seen)
}

fn sniff(c: &Client, url: &str, referer: &str, depth: u8, seen: &mut HashSet<String>) -> Option<(Vec<String>, String)> {
    if is_media_path(url) {
        return Some((vec![url.to_string()], referer.to_string()));
    }
    if depth == 0 || ytdlp_host(url) || !seen.insert(url.to_string()) {
        return None;
    }
    let n = HOP_N.fetch_add(1, Ordering::Relaxed) + 1;
    vlog(format!("  [hop {n}] GET {}", truncate(url, 160)));
    let html = match get(c, url, Some(referer)) {
        Ok(h) => h,
        Err(e) => {
            vlog(format!("     failed: {e:#}"));
            return None;
        }
    };
    if VERBOSE.load(Ordering::Relaxed) {
        let f = format!("waku-debug-hop-{n}.html");
        let _ = std::fs::write(&f, &html);
        vlog(format!("     {} bytes, saved to ./{f}", html.len()));
    }
    let txt = html.replace("\\/", "/").replace("\\u0026", "&").replace("&amp;", "&");
    let media = Regex::new(MEDIA_PAT).ok()?;
    let mut found: Vec<String> = Vec::new();
    for m in media.find_iter(&txt) {
        let u = m.as_str().to_string();
        if !found.contains(&u) {
            found.push(u);
        }
    }
    if !found.is_empty() {
        for u in &found {
            vlog(format!("     FOUND stream: {u}"));
        }
        return Some((found, url.to_string()));
    }
    // atob("...") blobs: decode and look inside
    if let Ok(re) = Regex::new(r#"atob\(\s*["']([A-Za-z0-9+/=_-]{16,})["']\s*\)"#) {
        for cap in re.captures_iter(&txt) {
            if let Some(dec) = b64_decode(&cap[1]).and_then(|b| String::from_utf8(b).ok()) {
                let dec = dec.replace("\\/", "/");
                if let Some(m) = media.find(&dec) {
                    vlog(format!("     FOUND stream (atob): {}", m.as_str()));
                    return Some((vec![m.as_str().to_string()], url.to_string()));
                }
            }
        }
    }
    let next: Vec<String> = candidates(&txt, url).into_iter().take(8).collect();
    vlog(format!("     {} link(s) to follow", next.len()));
    for u in next {
        if let Some(found) = sniff(c, &u, url, depth - 1, seen) {
            return Some(found);
        }
    }
    None
}

// ------------------------------------------------------------------ main

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

fn anime_label(a: &Anime) -> String {
    let n = match a.eps.len() {
        0 => "no episodes".to_string(),
        1 => "1 ep".to_string(),
        k => format!("{k} eps"),
    };
    let mut parts = vec![n];
    parts.extend(a.tags.iter().cloned());
    format!("{}  ·  {}", truncate(&a.title, 90), parts.join(" · "))
}

fn fuzzy(prompt: &str, labels: &[String], default: usize) -> Result<Option<usize>> {
    Ok(FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .items(labels)
        .default(default.min(labels.len().saturating_sub(1)))
        .interact_opt()?)
}

/// Directory containing our own executable.
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(|p| p.to_path_buf())
}

/// Find a bundled tool (mpv, yt-dlp) next to our executable — in the same
/// folder or in ./mpv, ./bin, ./tools — before falling back to PATH.
/// On Windows `.com` is tried first so mpv prints to the console.
fn find_tool(name: &str) -> PathBuf {
    let exts: &[&str] = if cfg!(windows) { &[".com", ".exe"] } else { &[""] };
    if let Some(dir) = exe_dir() {
        for sub in ["", "mpv", "bin", "tools"] {
            for ext in exts {
                let p = dir.join(sub).join(format!("{name}{ext}"));
                if p.is_file() {
                    return p;
                }
            }
        }
    }
    PathBuf::from(name)
}

/// Run the player once. Ok(true) = played fine, Ok(false) = mpv could not
/// load the file (exit code 2) so the caller may try another server.
fn launch(cli: &Cli, target: &str, referer: &str, name: &str, direct: bool) -> Result<bool> {
    let player = if cli.player_cmd.contains(['/', '\\', '.']) {
        PathBuf::from(&cli.player_cmd) // explicit path or file name given
    } else {
        find_tool(&cli.player_cmd)
    };
    let mut cmd = Command::new(&player);
    let ytdlp = find_tool("yt-dlp");
    if ytdlp.is_file() {
        cmd.arg(format!("--script-opts=ytdl_hook-ytdl_path={}", ytdlp.display()));
    }
    if direct {
        // already a stream URL: skip yt-dlp, and let ffmpeg's HLS demuxer accept
        // playlists/segments with odd extensions (.txt playlist, .html segments)
        cmd.arg("--ytdl=no").arg("--demuxer-lavf-o=allowed_extensions=ALL");
        if target.to_lowercase().split('?').next().unwrap_or("").ends_with(".txt") {
            cmd.arg("--demuxer-lavf-format=hls");
        }
    }
    if !cli.show_warnings {
        // the host's HLS segments have continuity-counter gaps at segment joins;
        // ffmpeg logs "Packet corrupt" for each one although playback is fine
        cmd.arg("--msg-level=ffmpeg/demuxer=error");
    }
    let status = cmd
        // Cloudflare TLS-fingerprint checks: let yt-dlp impersonate a browser
        // (needs curl_cffi — the official yt-dlp.exe already bundles it),
        // plus a longer timeout for slow hosts such as ok.ru.
        .arg("--ytdl-raw-options=extractor-args=generic:impersonate,socket-timeout=60,retries=3")
        .arg(format!("--referrer={referer}"))
        .arg(format!("--user-agent={UA}"))
        .arg(format!("--force-media-title={name}"))
        .arg(target)
        .status()
        .with_context(|| format!("failed to run '{}' — put mpv next to this program (./mpv/mpv.exe) or install it in PATH", player.display()))?;
    if !status.success() {
        eprintln!("{} exited with {status}", cli.player_cmd);
    }
    // mpv: 2 = file could not be loaded; anything else non-zero (e.g. user quit) is not retried
    Ok(status.success() || status.code() != Some(2))
}

// ------------------------------------------------------------ quality

#[derive(Debug, Clone)]
struct Variant {
    height: Option<u32>, // 1080, 720 … (None = unknown / adaptive)
    url: String,
}

/// Height encoded in a playlist URL: `…-1080.txt`, `…_720p.m3u8`, `…/480.mp4`
fn quality_of(url: &str) -> Option<u32> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)[-_/.](\d{3,4})p?\.(?:txt|m3u8|mp4)(?:\?|$)").unwrap());
    re.captures(url)
        .and_then(|c| c[1].parse::<u32>().ok())
        .filter(|h| (144..=4320).contains(h))
}

/// Same playlist URL with another height (`…-1080.txt` -> `…-720.txt`).
fn quality_sibling(url: &str, h: u32) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)^(.*[-_/.])(\d{3,4})(p?)(\.(?:txt|m3u8)(?:\?.*)?)$").unwrap());
    let c = re.captures(url)?;
    Some(format!("{}{}{}{}", &c[1], h, &c[3], &c[4]))
}

/// Is this URL really an HLS playlist? (fetch it and look for #EXTM3U)
fn probe_playlist(c: &Client, url: &str, referer: &str) -> bool {
    match get(c, url, Some(referer)) {
        Ok(body) => {
            let ok = body.trim_start().starts_with("#EXTM3U");
            vlog(format!("     probe {} -> {}", truncate(url, 110), if ok { "playlist" } else { "not a playlist" }));
            ok
        }
        Err(e) => {
            vlog(format!("     probe {} -> {e:#}", truncate(url, 110)));
            false
        }
    }
}

/// Every quality we can offer for a stream: the URLs found on the page, plus
/// sibling heights (1080/720/480/360) that answer with a real playlist.
fn find_variants(c: &Client, urls: &[String], page: &str) -> Vec<Variant> {
    let mut v: Vec<Variant> = Vec::new();
    for u in urls {
        if !v.iter().any(|x| x.url == *u) {
            v.push(Variant { height: quality_of(u), url: u.clone() });
        }
    }
    let base = urls.iter().find(|u| quality_of(u).is_some() && quality_sibling(u, 720).is_some());
    if let Some(base) = base {
        let referer = origin_of(page);
        let wanted: Vec<u32> = [1080u32, 720, 480, 360]
            .into_iter()
            .filter(|h| !v.iter().any(|x| x.height == Some(*h)))
            .collect();
        let extra: Vec<Variant> = par_map(&wanted, |&h| {
            quality_sibling(base, h)
                .filter(|u| probe_playlist(c, u, &referer))
                .map(|u| Variant { height: Some(h), url: u })
        })
        .into_iter()
        .flatten()
        .collect();
        v.extend(extra);
    }
    v.sort_by(|a, b| b.height.cmp(&a.height)); // best first, unknown last
    v
}

/// "best" | "worst" | "1080" | "720p" … -> closest available variant.
fn pick_variant<'a>(vs: &'a [Variant], want: &str) -> &'a Variant {
    let w = want.trim().to_lowercase();
    let w = w.trim_end_matches('p');
    let known = || vs.iter().filter(|v| v.height.is_some());
    match w {
        "" | "best" | "auto" | "max" => &vs[0],
        "worst" | "min" | "low" => known().last().unwrap_or(&vs[0]),
        n => match n.parse::<u32>() {
            // highest not above the request, else the lowest one available
            Ok(h) => known().find(|v| v.height.map_or(false, |x| x <= h)).or_else(|| known().last()).unwrap_or(&vs[0]),
            Err(_) => &vs[0],
        },
    }
}

fn settings_file() -> PathBuf {
    cache_file().with_file_name("settings.conf")
}

fn load_quality() -> Option<String> {
    let t = std::fs::read_to_string(settings_file()).ok()?;
    t.lines()
        .find_map(|l| l.strip_prefix("quality="))
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty())
}

fn save_quality(q: &str) {
    let f = settings_file();
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(f, format!("quality={q}\n"));
}

/// State kept between episodes in one run.
struct Session {
    pref: Option<usize>, // server that worked last
    quality: String,     // best | worst | 1080p | …
    avail: Vec<u32>,     // heights offered by the last server used
}

// ------------------------------------------------------------ playing

/// One server, resolved as far as we can: where to point mpv and how.
struct Resolved {
    idx: usize,
    name: String,
    page: String,          // page the stream was found on (or the embed itself)
    variants: Vec<Variant>, // qualities, best first (never empty)
    direct: bool,          // true = real stream URL(s) (no yt-dlp needed)
    rank: u8,              // 0 direct stream, 1 yt-dlp host, 2 unknown page (last resort)
}

fn resolve_server(c: &Client, ps: &[PlayerOpt], i: usize, ep_url: &str) -> Result<Resolved> {
    let embed = resolve(c, &ps[i], ep_url)?;
    let name = ps[i].name.clone();
    Ok(match sniff_stream(c, &embed, ep_url) {
        Some((urls, page)) => {
            let variants = find_variants(c, &urls, &page);
            Resolved { idx: i, name, page, variants, direct: true, rank: 0 }
        }
        None => {
            let rank = if ytdlp_host(&embed) { 1 } else { 2 };
            Resolved { idx: i, name, page: ep_url.to_string(), variants: vec![Variant { height: None, url: embed }], direct: false, rank }
        }
    })
}

fn play(c: &Client, cli: &Cli, ep: &Episode, title: &str, sess: &mut Session) -> Result<()> {
    let ps = players(c, &ep.url)?;
    let idxs: Vec<usize> = (0..ps.len()).collect();
    eprintln!("Checking {} server(s) ...", ps.len());
    let results: Vec<(usize, Result<Resolved>)> = par_map(&idxs, |&i| (i, resolve_server(c, &ps, i, &ep.url)));

    let mut ok: Vec<Resolved> = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    for (i, r) in results {
        match r {
            Ok(x) => ok.push(x),
            Err(e) => errs.push(format!("{}: {e:#}", ps[i].name)),
        }
    }
    if ok.is_empty() {
        bail!("no server could be resolved for {}:\n  {}", ep.url, errs.join("\n  "));
    }

    // best first: forced server (-p N) > direct stream > yt-dlp host > unknown page;
    // the server that worked last time wins ties
    let forced = cli.player.map(|n| n.wrapping_sub(1));
    let soft = sess.pref.map(|n| n.wrapping_sub(1));
    ok.sort_by_key(|r| (Some(r.idx) != forced, r.rank, Some(r.idx) != soft, r.idx));

    if cli.choose_server {
        let labels: Vec<String> = ok
            .iter()
            .map(|r| {
                let kind = match r.rank {
                    0 => "direct stream",
                    1 => "via yt-dlp",
                    _ => "unknown host (last resort)",
                };
                let qs: Vec<String> = r.variants.iter().filter_map(|v| v.height).map(|h| format!("{h}p")).collect();
                if qs.is_empty() {
                    format!("{}  [{kind}]", r.name)
                } else {
                    format!("{}  [{kind}: {}]", r.name, qs.join(" "))
                }
            })
            .collect();
        match fuzzy("Select server", &labels, 0)? {
            Some(i) => {
                let r = ok.remove(i);
                ok.insert(0, r);
            }
            None => return Ok(()),
        }
    }

    // heights the quality menu can offer
    let mut avail: Vec<u32> = ok[0].variants.iter().filter_map(|v| v.height).collect();
    avail.dedup();
    if !avail.is_empty() {
        sess.avail = avail;
    }

    let name = format!("{title} — {}", ep.label);
    for (n, r) in ok.iter().enumerate() {
        let v = pick_variant(&r.variants, &sess.quality).clone();
        if cli.print {
            println!("{}", v.url);
            return Ok(());
        }
        if n > 0 {
            eprintln!("↻ trying next server: {}", r.name);
        }
        if r.rank == 2 {
            eprintln!("  (no direct stream found on {} — last resort via yt-dlp)", r.name);
        }
        let q = v.height.map(|h| format!("{h}p")).unwrap_or_else(|| "auto".into());
        eprintln!("▶ {name}  [{} · {q}]\n  {}", r.name, v.url);

        // Referer candidates: browser-style origin of the page, origin of the stream, full page
        let mut refs: Vec<String> = vec![origin_of(&r.page)];
        if r.direct {
            for extra in [origin_of(&v.url), r.page.clone()] {
                if !refs.contains(&extra) {
                    refs.push(extra);
                }
            }
        }
        for rf in &refs {
            if launch(cli, &v.url, rf, &name, r.direct)? {
                sess.pref = Some(r.idx + 1);
                return Ok(());
            }
        }
    }
    sess.pref = None;
    bail!("all servers failed for {}\n  {}", ep.url, errs.join("\n  "))
}

/// Episode picker + next/prev/replay loop. Returns true to go back to results.
fn watch(c: &Client, cli: &Cli, anime: &Anime) -> Result<bool> {
    if anime.eps.is_empty() {
        eprintln!("No episodes listed for this title yet.");
        return Ok(true);
    }
    let labels: Vec<String> = anime.eps.iter().map(|e| e.label.clone()).collect();
    let mut cur = if let Some(n) = cli.episode {
        match anime.eps.iter().position(|e| (e.num - n).abs() < f32::EPSILON) {
            Some(i) => i,
            None => bail!("episode {n} not found"),
        }
    } else {
        let prompt = format!("{} — select episode ({} total, Esc = back)", truncate(&anime.title, 60), anime.eps.len());
        match fuzzy(&prompt, &labels, 0)? {
            Some(i) => i,
            None => return Ok(true),
        }
    };
    let mut sess = Session {
        pref: None,
        quality: cli.quality.clone().or_else(load_quality).unwrap_or_else(|| "best".into()),
        avail: Vec::new(),
    };
    loop {
        if let Err(e) = play(c, cli, &anime.eps[cur], &anime.title, &mut sess) {
            eprintln!("error: {e:#}");
        }
        if cli.print || cli.episode.is_some() {
            return Ok(false);
        }
        let mut menu: Vec<(String, u8)> = Vec::new();
        if cur + 1 < anime.eps.len() {
            menu.push(("Next episode".into(), 0));
        }
        if cur > 0 {
            menu.push(("Previous episode".into(), 1));
        }
        menu.push(("Replay".into(), 2));
        menu.push((format!("Change quality (now: {})", sess.quality), 6));
        menu.push(("Pick another episode".into(), 3));
        menu.push(("Back to results".into(), 4));
        menu.push(("Quit".into(), 5));
        let items: Vec<&str> = menu.iter().map(|m| m.0.as_str()).collect();
        let pick = Select::with_theme(&ColorfulTheme::default())
            .with_prompt(anime.eps[cur].label.as_str())
            .items(&items)
            .default(0)
            .interact_opt()?;
        match pick.map(|i| menu[i].1) {
            Some(0) => cur += 1,
            Some(1) => cur -= 1,
            Some(2) => {}
            Some(3) => {
                if let Some(i) = fuzzy("Select episode", &labels, cur)? {
                    cur = i;
                }
            }
            Some(4) => return Ok(true),
            Some(6) => {
                let mut opts: Vec<String> = vec!["best".into()];
                let heights: Vec<u32> = if sess.avail.is_empty() { vec![1080, 720, 480, 360] } else { sess.avail.clone() };
                opts.extend(heights.iter().map(|h| format!("{h}p")));
                opts.push("worst".into());
                let now = opts.iter().position(|o| *o == sess.quality).unwrap_or(0);
                if let Some(i) = fuzzy("Select quality (the episode replays)", &opts, now)? {
                    sess.quality = opts[i].clone();
                    save_quality(&sess.quality);
                }
                // loop again: replays the current episode in the new quality
            }
            _ => return Ok(false),
        }
    }
}

fn debug_ep(c: &Client, url: &str) -> Result<()> {
    let html = get(c, url, Some(base()))?;
    let out = "waku-debug-ep.html";
    std::fs::write(out, &html)?;
    println!("saved {} bytes of the episode page to ./{out}", html.len());
    VERBOSE.store(true, Ordering::Relaxed);
    let ps = players(c, url)?;
    println!("\nplayers found: {}", ps.len());
    for (i, p) in ps.iter().enumerate() {
        println!("  {}. {} -> {:?}", i + 1, p.name, p.kind);
        match resolve(c, p, url) {
            Ok(e) => {
                println!("     resolved: {e}");
                match sniff_stream(c, &e, url) {
                    Some((urls, page)) => {
                        println!("     direct stream(s):");
                        for u in &urls {
                            println!("       {u}");
                        }
                        println!("     found on: {page}");
                        let vs = find_variants(c, &urls, &page);
                        let qs: Vec<String> = vs.iter().map(|v| v.height.map_or("?".to_string(), |h| format!("{h}p"))).collect();
                        println!("     qualities: {}", qs.join(", "));
                    }
                    None if ytdlp_host(&e) => println!("     (host handled by mpv/yt-dlp)"),
                    None => {
                        println!("     no direct stream found; embed page contents:");
                        match get(c, &e, Some(url)) {
                            Ok(h) => {
                                let f = format!("waku-debug-player-{}.html", i + 1);
                                let _ = std::fs::write(&f, &h);
                                println!("     saved {} bytes to ./{f}", h.len());
                                let key = Regex::new(r"(?i)(<iframe|m3u8|\.mp4|file|source|atob|eval\(|decrypt|fetch\(|\.ajax|hls|player)")?;
                                for line in h.lines().filter(|l| key.is_match(l)).take(15) {
                                    println!("     | {}", truncate(line.trim(), 220));
                                }
                            }
                            Err(er) => println!("     fetch failed: {er:#}"),
                        }
                    }
                }
            }
            Err(e) => println!("     FAILED: {e:#}"),
        }
    }
    println!("\nplayer-related lines in the page:");
    let key = Regex::new(r"(?i)(dooplay|doo_player|admin-ajax|<iframe|m3u8|\.mp4|data-nume|embed)")?;
    let mut n = 0;
    for line in html.lines() {
        if key.is_match(line) && n < 25 {
            println!("  | {}", truncate(line.trim(), 220));
            n += 1;
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let c = client()?;

    if let Some(u) = &cli.debug_ep {
        return debug_ep(&c, u);
    }

    if let Some(u) = &cli.url {
        let a = load_anime(&c, u, "")?;
        watch(&c, &cli, &a)?;
        return Ok(());
    }

    let index = load_index(&c, cli.reindex);
    let mut query = cli.query.join(" ");
    loop {
        if query.trim().is_empty() {
            if cli.reindex && cli.query.is_empty() && !cli.list {
                // `waku --reindex` alone just refreshes the index
                if std::env::args().len() == 2 {
                    return Ok(());
                }
            }
            query = Input::<String>::with_theme(&ColorfulTheme::default())
                .with_prompt("Search anime")
                .interact_text()?;
        }
        eprintln!("Searching '{query}' ...");
        let results = find_anime(&c, &query, &index);

        if cli.list {
            for a in &results {
                println!("{:.2}\t{}\t{}\t{}", a.score, a.eps.len(), a.title, a.url);
            }
            return Ok(());
        }
        if results.is_empty() {
            eprintln!("No results for '{query}'. Try another spelling, or run `waku --reindex`.");
            query.clear();
            continue;
        }
        let labels: Vec<String> = results.iter().map(anime_label).collect();
        loop {
            let prompt = format!("{} result(s) — select anime (type to filter, Esc = quit)", results.len());
            let Some(i) = fuzzy(&prompt, &labels, 0)? else { return Ok(()) };
            if !watch(&c, &cli, &results[i])? {
                return Ok(());
            }
            if cli.print {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vs(hs: &[Option<u32>]) -> Vec<Variant> {
        hs.iter().map(|h| Variant { height: *h, url: format!("u{h:?}") }).collect()
    }

    #[test]
    fn quality_from_url() {
        let u = "https://player-ok-goal.doodee-player.com/m3u8/e74ffcac72d2c21106f7169db90191c7-480.txt";
        assert_eq!(quality_of(u), Some(480));
        assert_eq!(quality_of("https://x.com/a/720p.m3u8?t=1"), Some(720));
        assert_eq!(quality_of("https://x.com/a/master.m3u8"), None);
        assert_eq!(
            quality_sibling(u, 1080).as_deref(),
            Some("https://player-ok-goal.doodee-player.com/m3u8/e74ffcac72d2c21106f7169db90191c7-1080.txt")
        );
        assert_eq!(quality_sibling("https://x.com/a/720p.m3u8?t=1", 360).as_deref(), Some("https://x.com/a/360p.m3u8?t=1"));
        assert_eq!(quality_sibling("https://x.com/a/clip.mp4", 360), None);
    }

    #[test]
    fn picks_closest_quality() {
        let v = vs(&[Some(1080), Some(720), Some(480), Some(360)]);
        assert_eq!(pick_variant(&v, "best").height, Some(1080));
        assert_eq!(pick_variant(&v, "auto").height, Some(1080));
        assert_eq!(pick_variant(&v, "worst").height, Some(360));
        assert_eq!(pick_variant(&v, "720p").height, Some(720));
        assert_eq!(pick_variant(&v, "720").height, Some(720));
        assert_eq!(pick_variant(&v, "600").height, Some(480)); // highest not above
        assert_eq!(pick_variant(&v, "144").height, Some(360)); // nothing lower: lowest available
        assert_eq!(pick_variant(&v, "2160").height, Some(1080));
        assert_eq!(pick_variant(&v, "nonsense").height, Some(1080));
        let single = vs(&[None]);
        assert_eq!(pick_variant(&single, "720").height, None);
        assert_eq!(pick_variant(&single, "worst").height, None);
    }

    #[test]
    fn media_pattern_and_candidates() {
        let page = r#"x "file":"https://h.com/m3u8/abc-1080.txt" y 'https://h.com/m3u8/abc-480.txt?x=1' z"#;
        let m: Vec<&str> = Regex::new(MEDIA_PAT).unwrap().find_iter(page).map(|m| m.as_str()).collect();
        assert_eq!(m, vec!["https://h.com/m3u8/abc-1080.txt", "https://h.com/m3u8/abc-480.txt?x=1"]);
        let json = r#"{"src": "https://private-okru.doodee-player.com/?token=ab+/c=", "img": "https://a.com/list.png"}"#;
        let c = candidates(json, "https://anime-waku.com/player/x/");
        assert_eq!(c, vec!["https://private-okru.doodee-player.com/?token=ab+/c="]);
    }

    #[test]
    fn base64() {
        assert_eq!(b64_decode("aHR0cDovL2E=").unwrap(), b"http://a");
    }
}
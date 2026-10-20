// Spotipad daemon for iPad mini 1 (iOS 9, armv7).
//
// librespot session + player -> AudioQueue (aq.c), plus a small HTTP server (:8888) serving an
// embedded web UI and a JSON API. Runs as a launchd daemon, 100% on the iPad.
//
// Performance design:
// - Everything the UI lists comes from an in-memory cache persisted to disk (cache.json), so
//   views open with zero network after the first warmup, even across daemon restarts.
// - Playlists are keyed by revision: the rootlist carries each playlist's revision, so an
//   unchanged playlist is never refetched. Other views are stale-while-revalidate.
// - Track metadata is fetched only for unknown URIs, in parallel batches.
// - Warmup at startup (and every 15 min) prefetches the rootlist, every playlist, liked songs
//   and all their metadata.
// - Cover art is proxied and cached on disk; the UI uses 64px images.
// - Track changes flush the AudioQueue so skips are instant.
use std::collections::{HashMap, HashSet};
use std::os::raw::{c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{stream, StreamExt};
use http::header::{HeaderValue, ACCEPT, CONTENT_TYPE};
use http::{HeaderMap, Method, Request};
use librespot_core::{
    cache::Cache,
    config::SessionConfig,
    session::Session,
    spotify_id::{SpotifyId, SpotifyItemType},
};
use librespot_playback::audio_backend::{Sink, SinkAsBytes, SinkError, SinkResult};
use librespot_playback::config::{Bitrate, PlayerConfig};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::mixer::NoOpVolume;
use librespot_playback::player::{Player, PlayerEvent};
use librespot_protocol::{
    collection2v2 as coll, extended_metadata as em, extension_kind::ExtensionKind, metadata as pm,
    playlist4_external as pl,
};
use log::{error, info, warn};
use protobuf::Message;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const INDEX_HTML: &str = include_str!("index.html");
const META_BATCH: usize = 200;
const META_PARALLEL: usize = 4;
const PLAYLIST_PARALLEL: usize = 3;
const ROOT_TTL: Duration = Duration::from_secs(30);
const LIKED_TTL: Duration = Duration::from_secs(30);
const WARMUP_EVERY: Duration = Duration::from_secs(15 * 60);

// ---------------------------------------------------------------- audio sink (AudioQueue)
extern "C" {
    fn aq_new(sample_rate: c_int, channels: c_int) -> *mut c_void;
    fn aq_write(h: *mut c_void, data: *const u8, len: c_int);
    fn aq_pause(h: *mut c_void);
    fn aq_resume(h: *mut c_void);
    fn aq_flush(h: *mut c_void);
}

static AQ: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

fn audio_flush() {
    let p = AQ.load(Ordering::Acquire);
    if !p.is_null() {
        unsafe { aq_flush(p) };
    }
}

struct AqSink;

impl Sink for AqSink {
    fn start(&mut self) -> SinkResult<()> {
        let p = AQ.load(Ordering::Acquire);
        if p.is_null() {
            let p = unsafe { aq_new(44100, 2) };
            if p.is_null() {
                return Err(SinkError::NotConnected("aq_new failed".into()));
            }
            AQ.store(p, Ordering::Release);
        } else {
            unsafe { aq_resume(p) };
        }
        Ok(())
    }
    fn stop(&mut self) -> SinkResult<()> {
        let p = AQ.load(Ordering::Acquire);
        if !p.is_null() {
            unsafe { aq_pause(p) };
        }
        Ok(())
    }
    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        if let AudioPacket::Samples(samples) = packet {
            let b = converter.f64_to_s16(&samples);
            let p = AQ.load(Ordering::Acquire);
            if !p.is_null() {
                unsafe { aq_write(p, b.as_ptr() as *const u8, (b.len() * 2) as c_int) };
            }
        }
        Ok(())
    }
}

impl SinkAsBytes for AqSink {
    fn write_bytes(&mut self, _data: &[u8]) -> SinkResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------- data model
#[derive(Clone, Serialize, Deserialize, Default)]
struct Item {
    uri: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    img: String,
    #[serde(default)]
    dur: u32,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct ListCache {
    rev: String,
    name: String,
    uris: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct RootCache {
    tree: Value,
    revs: HashMap<String, String>,
}

/// Persisted to <dir>/cache.json.
#[derive(Clone, Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    meta: HashMap<String, Item>,
    #[serde(default)]
    lists: HashMap<String, ListCache>, // "playlist:<id>", "album:<id>"
    #[serde(default)]
    root: Option<RootCache>,
    #[serde(default)]
    liked: Option<Vec<String>>,
}

#[derive(Default)]
struct Queue {
    items: Vec<Item>,
    idx: usize,
    playing: bool,
    loading: bool,
    pos_ms: u32,
    pos_at: Option<Instant>,
    req_id: u64,
    error: String,
    ctx: String,
}

struct App {
    dir: PathBuf,
    session: Mutex<Session>,
    player: Arc<Player>,
    cache: Cache,
    cfg: SessionConfig,
    q: Mutex<Queue>,
    store: Mutex<Store>,
    dirty: AtomicBool,
    root_at: Mutex<Option<Instant>>,
    liked_at: Mutex<Option<Instant>>,
    inflight: Mutex<HashSet<String>>,
    searches: Mutex<HashMap<String, Vec<String>>>,
}

fn hex(b: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(b)
}

/// 64px cover from an ImageGroup (iPad mini 1 is non-retina; rows are 48px), proxied by /img.
fn cover(g: Option<&pm::ImageGroup>) -> String {
    let Some(g) = g else { return String::new() };
    let pick = g
        .image
        .iter()
        .find(|i| i.size() == pm::image::Size::SMALL)
        .or_else(|| g.image.iter().find(|i| i.size() == pm::image::Size::DEFAULT))
        .or_else(|| g.image.first());
    pick.map(|i| format!("/img/{}", hex(i.file_id()))).unwrap_or_default()
}

fn collect_imgs(v: &Value, out: &mut HashSet<String>) {
    if let Some(items) = v["items"].as_array() {
        for it in items {
            if let Some(i) = it["img"].as_str() {
                if !i.is_empty() {
                    out.insert(i.to_string());
                }
            }
            collect_imgs(it, out);
        }
    }
}

fn track_id(uri: &str) -> Option<SpotifyId> {
    let mut id = SpotifyId::from_uri(uri).ok()?;
    if id.item_type == SpotifyItemType::Unknown {
        id.item_type = SpotifyItemType::Track;
    }
    Some(id)
}

fn decode_name(s: &str) -> String {
    url::form_urlencoded::parse(format!("n={}", s).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

fn ms(t: Instant) -> u128 {
    t.elapsed().as_millis()
}

type Frame = (String, String, Vec<Value>); // (folder id, folder name, children)

fn close_folder(stack: &mut Vec<Frame>) {
    if stack.len() > 1 {
        let (id, name, items) = stack.pop().unwrap();
        let n = items.len();
        stack
            .last_mut()
            .unwrap()
            .2
            .push(json!({ "t": "f", "id": id, "name": name, "n": n, "items": items }));
    }
}

/// Rootlist -> folder tree. Folders are `spotify:start-group:<id>:<name>` ...
/// `spotify:end-group:<id>` brackets around their items; meta_items is index-aligned with items.
fn parse_rootlist(c: &pl::SelectedListContent, user: &str) -> RootCache {
    let contents = c.contents.get_or_default();
    let mut revs = HashMap::new();
    let mut stack: Vec<Frame> = vec![(String::new(), String::new(), Vec::new())];
    for (i, it) in contents.items.iter().enumerate() {
        let uri = it.uri();
        if let Some(rest) = uri.strip_prefix("spotify:start-group:") {
            let (gid, name) = rest.split_once(':').unwrap_or((rest, ""));
            stack.push((gid.to_string(), decode_name(name), Vec::new()));
        } else if uri.starts_with("spotify:end-group:") {
            close_folder(&mut stack);
        } else if let Some(id) = uri.strip_prefix("spotify:playlist:") {
            let meta = contents.meta_items.get(i);
            let attrs = meta.and_then(|m| m.attributes.as_ref());
            let name = attrs
                .map(|a| a.name().to_string())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "Playlist".into());
            let img = attrs
                .filter(|a| a.has_picture())
                .map(|a| format!("/img/{}", hex(a.picture())))
                .unwrap_or_default();
            let owner = meta.map(|m| m.owner_username().to_string()).unwrap_or_default();
            let len = meta.map(|m| m.length()).unwrap_or(0);
            let sub = if owner.is_empty() || owner == user {
                format!("{} tracks", len)
            } else {
                format!("{} tracks · followed", len)
            };
            if let Some(m) = meta {
                if m.has_revision() {
                    revs.insert(id.to_string(), hex(m.revision()));
                }
            }
            stack
                .last_mut()
                .unwrap()
                .2
                .push(json!({ "t": "p", "id": id, "name": name, "img": img, "sub": sub }));
        }
    }
    while stack.len() > 1 {
        close_folder(&mut stack);
    }
    let items = stack.pop().unwrap().2;
    RootCache { tree: json!({ "items": items }), revs }
}

impl App {
    fn session(&self) -> Session {
        self.session.lock().unwrap().clone()
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    fn load_store(dir: &PathBuf) -> Store {
        let t = Instant::now();
        let s = std::fs::read(dir.join("cache.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Store>(&b).ok())
            .unwrap_or_default();
        info!("cache loaded: {} tracks, {} lists in {} ms", s.meta.len(), s.lists.len(), ms(t));
        s
    }

    fn save_store(&self) {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let t = Instant::now();
        let snapshot = self.store.lock().unwrap().clone();
        let Ok(bytes) = serde_json::to_vec(&snapshot) else { return };
        let tmp = self.dir.join("cache.json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, self.dir.join("cache.json"));
        }
        info!("cache saved: {} KB in {} ms", bytes.len() / 1024, ms(t));
    }

    // ------------------------------------------------ playback
    fn load_current(&self, start: bool) {
        let mut q = self.q.lock().unwrap();
        let Some(item) = q.items.get(q.idx).cloned() else { return };
        match track_id(&item.uri) {
            Some(id) => {
                q.loading = true;
                q.pos_ms = 0;
                q.pos_at = None;
                q.error.clear();
                drop(q);
                audio_flush();
                self.player.load(id, start, 0);
            }
            None => q.error = format!("invalid uri: {}", item.uri),
        }
    }

    fn skip(&self, delta: isize) -> bool {
        let mut q = self.q.lock().unwrap();
        let n = q.items.len() as isize;
        let next = q.idx as isize + delta;
        if n == 0 || next < 0 || next >= n {
            q.playing = false;
            return false;
        }
        q.idx = next as usize;
        drop(q);
        self.load_current(true);
        true
    }

    async fn ensure_session(&self) -> Result<(), String> {
        if !self.session().is_invalid() {
            return Ok(());
        }
        warn!("session invalid, reconnecting");
        let creds = self.cache.credentials().ok_or("no saved credentials")?;
        let s = Session::new(self.cfg.clone(), Some(self.cache.clone()));
        s.connect(creds, true).await.map_err(|e| e.to_string())?;
        self.player.set_session(s.clone());
        *self.session.lock().unwrap() = s;
        info!("reconnected");
        Ok(())
    }

    // ------------------------------------------------ network primitives
    async fn sp_get(&self, path: &str) -> Result<Bytes, String> {
        self.ensure_session().await?;
        self.session()
            .spclient()
            .request(&Method::GET, path, None, None)
            .await
            .map_err(|e| e.to_string())
    }

    /// spclient GET without the product/country/salt metrics params (they leak into
    /// context-resolve search queries).
    async fn sp_get_plain(&self, path: &str) -> Result<Bytes, String> {
        self.ensure_session().await?;
        let s = self.session();
        let base = s.spclient().base_url().await.map_err(|e| e.to_string())?;
        let tok = s.login5().auth_token().await.map_err(|e| e.to_string())?;
        let mut req = Request::builder()
            .method(Method::GET)
            .uri(format!("{}{}", base, path))
            .header(http::header::AUTHORIZATION, format!("Bearer {}", tok.access_token))
            .header(ACCEPT, "application/json");
        if let Ok(ct) = s.spclient().client_token().await {
            req = req.header("client-token", ct);
        }
        let req = req.body(Bytes::new()).map_err(|e| e.to_string())?;
        s.http_client().request_body(req).await.map_err(|e| e.to_string())
    }

    async fn ext_meta_chunk(&self, uris: Vec<String>, kind: ExtensionKind) -> Result<Vec<(String, Bytes)>, String> {
        let req = em::BatchedEntityRequest {
            entity_request: uris
                .iter()
                .map(|u| em::EntityRequest {
                    entity_uri: u.clone(),
                    query: vec![em::ExtensionQuery {
                        extension_kind: protobuf::EnumOrUnknown::new(kind),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let res = self
            .session()
            .spclient()
            .request_with_protobuf(&Method::POST, "/extended-metadata/v0/extended-metadata", None, &req)
            .await
            .map_err(|e| e.to_string())?;
        let res = em::BatchedExtensionResponse::parse_from_bytes(&res).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for arr in res.extended_metadata {
            for d in arr.extension_data {
                if let Some(any) = d.extension_data.as_ref() {
                    out.push((d.entity_uri.clone(), Bytes::from(any.value.clone())));
                }
            }
        }
        Ok(out)
    }

    /// Fetch metadata only for track URIs not in the cache, in parallel batches.
    async fn ensure_meta(&self, uris: &[String]) -> Result<(), String> {
        let missing: Vec<String> = {
            let st = self.store.lock().unwrap();
            let mut seen = HashSet::new();
            uris.iter()
                .filter(|u| u.starts_with("spotify:track:") && !st.meta.contains_key(*u) && seen.insert(*u))
                .cloned()
                .collect()
        };
        if missing.is_empty() {
            return Ok(());
        }
        self.ensure_session().await?;
        let t = Instant::now();
        let n = missing.len();
        let chunks: Vec<Vec<String>> = missing.chunks(META_BATCH).map(|c| c.to_vec()).collect();
        let results: Vec<_> = stream::iter(chunks)
            .map(|c| self.ext_meta_chunk(c, ExtensionKind::TRACK_V4))
            .buffer_unordered(META_PARALLEL)
            .collect()
            .await;
        let mut got = 0;
        let mut err = None;
        {
            let mut st = self.store.lock().unwrap();
            for r in results {
                match r {
                    Ok(v) => {
                        for (uri, b) in v {
                            let Ok(t) = pm::Track::parse_from_bytes(&b) else { continue };
                            st.meta.insert(
                                uri.clone(),
                                Item {
                                    uri,
                                    name: t.name().to_string(),
                                    artist: t.artist.iter().map(|a| a.name()).collect::<Vec<_>>().join(", "),
                                    img: cover(t.album.cover_group.as_ref()),
                                    dur: t.duration().max(0) as u32,
                                },
                            );
                            got += 1;
                        }
                    }
                    Err(e) => err = Some(e),
                }
            }
        }
        if got > 0 {
            self.mark_dirty();
        }
        info!("metadata: {}/{} tracks in {} ms", got, n, ms(t));
        match err {
            Some(e) if got == 0 => Err(e),
            _ => Ok(()),
        }
    }

    fn items_for(&self, uris: &[String]) -> Vec<Item> {
        let st = self.store.lock().unwrap();
        uris.iter().filter_map(|u| st.meta.get(u).cloned()).collect()
    }

    // ------------------------------------------------ cached sources
    async fn refresh_root(&self) -> Result<(), String> {
        let t = Instant::now();
        let user = self.session().username();
        let b = self
            .sp_get(&format!(
                "/playlist/v2/user/{}/rootlist?decorate=revision,attributes,length,owner&from=0&length=1000",
                user
            ))
            .await?;
        let c = pl::SelectedListContent::parse_from_bytes(&b).map_err(|e| e.to_string())?;
        let root = parse_rootlist(&c, &user);
        info!("rootlist: {} playlists in {} ms", root.revs.len(), ms(t));
        self.store.lock().unwrap().root = Some(root);
        *self.root_at.lock().unwrap() = Some(Instant::now());
        self.mark_dirty();
        Ok(())
    }

    async fn fetch_playlist(&self, id: &str) -> Result<(), String> {
        let t = Instant::now();
        let b = self.sp_get(&format!("/playlist/v2/playlist/{}?from=0&length=5000", id)).await?;
        let c = pl::SelectedListContent::parse_from_bytes(&b).map_err(|e| e.to_string())?;
        let lc = ListCache {
            rev: hex(c.revision()),
            name: c.attributes.as_ref().map(|a| a.name().to_string()).unwrap_or_default(),
            uris: c.contents.get_or_default().items.iter().map(|i| i.uri().to_string()).collect(),
        };
        info!("playlist {}: {} items in {} ms", id, lc.uris.len(), ms(t));
        self.store.lock().unwrap().lists.insert(format!("playlist:{}", id), lc);
        self.mark_dirty();
        Ok(())
    }

    /// Cached playlist is fresh when its revision matches the rootlist's.
    fn playlist_state(&self, id: &str) -> (Option<ListCache>, bool) {
        let st = self.store.lock().unwrap();
        let cached = st.lists.get(&format!("playlist:{}", id)).cloned();
        let root_rev = st.root.as_ref().and_then(|r| r.revs.get(id));
        let fresh = match (&cached, root_rev) {
            (Some(c), Some(r)) => &c.rev == r,
            _ => false,
        };
        (cached, fresh)
    }

    async fn fetch_liked(&self) -> Result<(), String> {
        self.ensure_session().await?;
        let t = Instant::now();
        let user = self.session().username();
        let mut all: Vec<coll::CollectionItem> = Vec::new();
        let mut token = String::new();
        for _ in 0..20 {
            let req = coll::PageRequest {
                username: user.clone(),
                set: "collection".into(),
                pagination_token: token.clone(),
                limit: 500,
                ..Default::default()
            };
            let body = req.write_to_bytes().map_err(|e| e.to_string())?;
            let mut h = HeaderMap::new();
            let ct = HeaderValue::from_static("application/vnd.collection-v2.spotify.proto");
            h.insert(CONTENT_TYPE, ct.clone());
            h.insert(ACCEPT, ct);
            let res = self
                .session()
                .spclient()
                .request_bytes(&Method::POST, "/collection/v2/paging", Some(h), &body)
                .await
                .map_err(|e| e.to_string())?;
            let page = coll::PageResponse::parse_from_bytes(&res).map_err(|e| e.to_string())?;
            all.extend(page.items.into_iter().filter(|i| !i.is_removed && i.uri.starts_with("spotify:track:")));
            if page.next_page_token.is_empty() {
                break;
            }
            token = page.next_page_token;
        }
        all.sort_by(|a, b| b.added_at.cmp(&a.added_at));
        let uris: Vec<String> = all.into_iter().map(|i| i.uri).collect();
        info!("liked: {} tracks in {} ms", uris.len(), ms(t));
        self.store.lock().unwrap().liked = Some(uris);
        *self.liked_at.lock().unwrap() = Some(Instant::now());
        self.mark_dirty();
        Ok(())
    }

    /// Run `job` in the background unless one with the same key is already running.
    fn background<F>(self: &Arc<Self>, key: String, job: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if !self.inflight.lock().unwrap().insert(key.clone()) {
            return;
        }
        let app = self.clone();
        tokio::spawn(async move {
            job.await;
            app.inflight.lock().unwrap().remove(&key);
        });
    }

    // ------------------------------------------------ API views
    async fn api_playlists(self: &Arc<Self>) -> Result<Value, String> {
        let cached = self.store.lock().unwrap().root.as_ref().map(|r| r.tree.clone());
        if let Some(tree) = cached {
            let stale = self.root_at.lock().unwrap().map_or(true, |t| t.elapsed() > ROOT_TTL);
            if stale {
                let app = self.clone();
                self.background("root".into(), async move {
                    if let Err(e) = app.refresh_root().await {
                        warn!("root refresh: {}", e);
                    }
                });
            }
            return Ok(tree);
        }
        self.refresh_root().await?;
        Ok(self.store.lock().unwrap().root.as_ref().map(|r| r.tree.clone()).unwrap_or(json!({"items": []})))
    }

    async fn playlist_uris(self: &Arc<Self>, id: &str) -> Result<(String, Vec<String>), String> {
        let (cached, fresh) = self.playlist_state(id);
        if let Some(c) = cached {
            if !fresh {
                let app = self.clone();
                let id2 = id.to_string();
                self.background(format!("pl:{}", id), async move {
                    if app.fetch_playlist(&id2).await.is_ok() {
                        let uris = app.playlist_state(&id2).0.map(|c| c.uris).unwrap_or_default();
                        let _ = app.ensure_meta(&uris).await;
                    }
                });
            }
            return Ok((c.name, c.uris));
        }
        self.fetch_playlist(id).await?;
        let c = self.playlist_state(id).0.ok_or("empty playlist")?;
        Ok((c.name, c.uris))
    }

    async fn api_playlist(self: &Arc<Self>, id: &str) -> Result<Value, String> {
        let (name, uris) = self.playlist_uris(id).await?;
        self.ensure_meta(&uris).await?;
        Ok(json!({ "name": name, "tracks": self.items_for(&uris) }))
    }

    async fn liked_uris(self: &Arc<Self>) -> Result<Vec<String>, String> {
        let cached = self.store.lock().unwrap().liked.clone();
        if let Some(u) = cached {
            let stale = self.liked_at.lock().unwrap().map_or(true, |t| t.elapsed() > LIKED_TTL);
            if stale {
                let app = self.clone();
                self.background("liked".into(), async move {
                    if app.fetch_liked().await.is_ok() {
                        let uris = app.store.lock().unwrap().liked.clone().unwrap_or_default();
                        let _ = app.ensure_meta(&uris).await;
                    }
                });
            }
            return Ok(u);
        }
        self.fetch_liked().await?;
        Ok(self.store.lock().unwrap().liked.clone().unwrap_or_default())
    }

    async fn api_liked(self: &Arc<Self>) -> Result<Value, String> {
        let uris = self.liked_uris().await?;
        self.ensure_meta(&uris).await?;
        Ok(json!({ "name": "Liked Songs", "tracks": self.items_for(&uris) }))
    }

    async fn album_uris(&self, id: &str) -> Result<(String, Vec<String>), String> {
        let key = format!("album:{}", id);
        if let Some(c) = self.store.lock().unwrap().lists.get(&key).cloned() {
            return Ok((c.name, c.uris));
        }
        self.ensure_session().await?;
        let uri = format!("spotify:album:{}", id);
        let res = self.ext_meta_chunk(vec![uri], ExtensionKind::ALBUM_V4).await?;
        let (_, b) = res.into_iter().next().ok_or("album has no metadata")?;
        let a = pm::Album::parse_from_bytes(&b).map_err(|e| e.to_string())?;
        let mut uris = Vec::new();
        for d in &a.disc {
            for t in &d.track {
                if let Ok(mut sid) = SpotifyId::from_raw(t.gid()) {
                    sid.item_type = SpotifyItemType::Track;
                    if let Ok(u) = sid.to_uri() {
                        uris.push(u);
                    }
                }
            }
        }
        let artist = a.artist.iter().map(|x| x.name()).collect::<Vec<_>>().join(", ");
        let name = format!("{} · {}", a.name(), artist);
        self.store
            .lock()
            .unwrap()
            .lists
            .insert(key, ListCache { rev: String::new(), name: name.clone(), uris: uris.clone() });
        self.mark_dirty();
        Ok((name, uris))
    }

    async fn api_album(&self, id: &str) -> Result<Value, String> {
        let (name, uris) = self.album_uris(id).await?;
        self.ensure_meta(&uris).await?;
        Ok(json!({ "name": name, "tracks": self.items_for(&uris) }))
    }

    /// Track search via context-resolve (`spotify:search:<words+joined>`); searchview is gone.
    async fn search_uris(&self, q: &str) -> Result<Vec<String>, String> {
        let key = q.trim().to_lowercase();
        if let Some(u) = self.searches.lock().unwrap().get(&key).cloned() {
            return Ok(u);
        }
        let words: Vec<String> = key
            .split_whitespace()
            .map(|w| url::form_urlencoded::byte_serialize(w.as_bytes()).collect::<String>())
            .collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let t = Instant::now();
        let b = self
            .sp_get_plain(&format!("/context-resolve/v1/spotify:search:{}", words.join("+")))
            .await?;
        let v: Value = serde_json::from_slice(&b).map_err(|e| format!("search json: {}", e))?;
        let mut uris = Vec::new();
        for page in v["pages"].as_array().into_iter().flatten() {
            for t in page["tracks"].as_array().into_iter().flatten() {
                if let Some(u) = t["uri"].as_str() {
                    let u = u.split('?').next().unwrap_or(u).to_string();
                    if !uris.contains(&u) {
                        uris.push(u);
                    }
                }
            }
        }
        uris.truncate(50);
        info!("search '{}': {} in {} ms", key, uris.len(), ms(t));
        let mut s = self.searches.lock().unwrap();
        if s.len() > 200 {
            s.clear();
        }
        s.insert(key, uris.clone());
        Ok(uris)
    }

    async fn api_search(&self, q: &str) -> Result<Value, String> {
        let uris = self.search_uris(q).await?;
        self.ensure_meta(&uris).await?;
        Ok(json!({ "tracks": self.items_for(&uris) }))
    }

    /// Resolve a play context to the exact list the UI shows.
    async fn ctx_items(self: &Arc<Self>, ctx: &str) -> Result<Vec<Item>, String> {
        let uris = if ctx == "liked" {
            self.liked_uris().await?
        } else if let Some(id) = ctx.strip_prefix("playlist:") {
            self.playlist_uris(id).await?.1
        } else if let Some(id) = ctx.strip_prefix("album:") {
            self.album_uris(id).await?.1
        } else if let Some(q) = ctx.strip_prefix("search:") {
            self.search_uris(q).await?
        } else {
            return Err(format!("unknown context: {}", ctx));
        };
        self.ensure_meta(&uris).await?;
        Ok(self.items_for(&uris))
    }

    // ------------------------------------------------ images (disk cached)
    async fn image(&self, hexid: &str) -> Result<Bytes, String> {
        if hexid.len() < 20 || hexid.len() > 64 || !hexid.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("bad id".into());
        }
        let path = self.dir.join("img").join(hexid);
        if let Ok(b) = std::fs::read(&path) {
            return Ok(Bytes::from(b));
        }
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("https://i.scdn.co/image/{}", hexid))
            .body(Bytes::new())
            .map_err(|e| e.to_string())?;
        let b = self.session().http_client().request_body(req).await.map_err(|e| e.to_string())?;
        let _ = std::fs::write(&path, &b);
        Ok(b)
    }

    /// Download every cover the UI can show (64px, ~3 KB each) so lists paint fully at once.
    async fn prefetch_images(&self) {
        let t = Instant::now();
        let ids: Vec<String> = {
            let st = self.store.lock().unwrap();
            let mut set: HashSet<String> = st.meta.values().map(|i| i.img.clone()).collect();
            if let Some(r) = &st.root {
                collect_imgs(&r.tree, &mut set);
            }
            set.into_iter()
                .filter_map(|p| p.strip_prefix("/img/").map(|s| s.to_string()))
                .filter(|h| !self.dir.join("img").join(h).exists())
                .collect()
        };
        if ids.is_empty() {
            return;
        }
        let n = ids.len();
        let ok: usize = stream::iter(ids)
            .map(|h| async move { self.image(&h).await.is_ok() as usize })
            .buffer_unordered(6)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .sum();
        info!("images: {}/{} prefetched in {} ms", ok, n, ms(t));
    }

    // ------------------------------------------------ warmup
    async fn warmup(self: &Arc<Self>) {
        let t = Instant::now();
        if let Err(e) = self.refresh_root().await {
            warn!("warmup root: {}", e);
        }
        let ids: Vec<String> = self
            .store
            .lock()
            .unwrap()
            .root
            .as_ref()
            .map(|r| r.revs.keys().cloned().collect())
            .unwrap_or_default();
        let stale: Vec<String> = ids.iter().filter(|id| !self.playlist_state(id).1).cloned().collect();
        let n_stale = stale.len();
        let _: Vec<_> = stream::iter(stale)
            .map(|id| async move {
                if let Err(e) = self.fetch_playlist(&id).await {
                    warn!("warmup playlist {}: {}", id, e);
                }
            })
            .buffer_unordered(PLAYLIST_PARALLEL)
            .collect()
            .await;
        if let Err(e) = self.fetch_liked().await {
            warn!("warmup liked: {}", e);
        }
        let all: Vec<String> = {
            let st = self.store.lock().unwrap();
            let mut v: Vec<String> = st.lists.values().flat_map(|l| l.uris.iter().cloned()).collect();
            v.extend(st.liked.clone().unwrap_or_default());
            v
        };
        if let Err(e) = self.ensure_meta(&all).await {
            warn!("warmup meta: {}", e);
        }
        // Drop cached lists for playlists no longer in the rootlist.
        {
            let mut st = self.store.lock().unwrap();
            let keep: HashSet<String> = ids.iter().map(|i| format!("playlist:{}", i)).collect();
            st.lists.retain(|k, _| !k.starts_with("playlist:") || keep.contains(k));
        }
        self.mark_dirty();
        self.save_store();
        self.prefetch_images().await;
        let tracks = self.store.lock().unwrap().meta.len();
        info!(
            "warmup done: {} playlists ({} refreshed), {} tracks cached, {} ms",
            ids.len(),
            n_stale,
            tracks,
            ms(t)
        );
    }

    fn status_json(&self) -> String {
        let q = self.q.lock().unwrap();
        let mut pos = q.pos_ms as u64;
        if q.playing {
            if let Some(t) = q.pos_at {
                pos += t.elapsed().as_millis() as u64;
            }
        }
        json!({
            "playing": q.playing,
            "loading": q.loading,
            "idx": q.idx,
            "count": q.items.len(),
            "item": q.items.get(q.idx),
            "pos_ms": pos,
            "error": q.error,
            "ctx": q.ctx,
            "connected": !self.session().is_invalid(),
        })
        .to_string()
    }
}

// ---------------------------------------------------------------- background tasks
async fn event_loop(app: Arc<App>) {
    let mut rx = app.player.get_player_event_channel();
    while let Some(ev) = rx.recv().await {
        match ev {
            PlayerEvent::PlayRequestIdChanged { play_request_id } => {
                app.q.lock().unwrap().req_id = play_request_id;
            }
            PlayerEvent::Playing { position_ms, .. } => {
                let next = {
                    let mut q = app.q.lock().unwrap();
                    let was_loading = q.loading;
                    q.playing = true;
                    q.loading = false;
                    q.pos_ms = position_ms;
                    q.pos_at = Some(Instant::now());
                    // Preload the next track as soon as this one starts, so a manual skip hits
                    // an already-buffered file instead of a cold key/CDN/download round trip.
                    if was_loading { q.items.get(q.idx + 1).and_then(|i| track_id(&i.uri)) } else { None }
                };
                if let Some(id) = next {
                    app.player.preload(id);
                }
            }
            PlayerEvent::Paused { position_ms, .. } => {
                let mut q = app.q.lock().unwrap();
                q.playing = false;
                q.loading = false;
                q.pos_ms = position_ms;
                q.pos_at = None;
            }
            PlayerEvent::Seeked { position_ms, .. } | PlayerEvent::PositionCorrection { position_ms, .. } => {
                let mut q = app.q.lock().unwrap();
                q.pos_ms = position_ms;
                q.pos_at = if q.playing { Some(Instant::now()) } else { None };
            }
            PlayerEvent::TimeToPreloadNextTrack { .. } => {
                let q = app.q.lock().unwrap();
                if let Some(it) = q.items.get(q.idx + 1) {
                    if let Some(id) = track_id(&it.uri) {
                        app.player.preload(id);
                    }
                }
            }
            PlayerEvent::EndOfTrack { play_request_id, .. } => {
                if play_request_id == app.q.lock().unwrap().req_id {
                    // Natural end: no flush, the tail of the track is still queued (gapless).
                    let next = {
                        let mut q = app.q.lock().unwrap();
                        if q.idx + 1 < q.items.len() {
                            q.idx += 1;
                            q.loading = true;
                            q.pos_ms = 0;
                            q.pos_at = None;
                            q.items.get(q.idx).and_then(|i| track_id(&i.uri))
                        } else {
                            q.playing = false;
                            None
                        }
                    };
                    if let Some(id) = next {
                        app.player.load(id, true, 0);
                    }
                }
            }
            PlayerEvent::Unavailable { play_request_id, track_id } => {
                warn!("unavailable: {:?}", track_id);
                if play_request_id == app.q.lock().unwrap().req_id {
                    app.q.lock().unwrap().error = "track unavailable, skipping".into();
                    app.skip(1);
                }
            }
            _ => {}
        }
    }
}

async fn housekeeping(app: Arc<App>) {
    let mut last_warm = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        if let Err(e) = app.ensure_session().await {
            error!("reconnect failed: {}", e);
            continue;
        }
        app.save_store();
        if last_warm.elapsed() > WARMUP_EVERY {
            last_warm = Instant::now();
            app.warmup().await;
        }
    }
}

// ---------------------------------------------------------------- http
struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_req(sock: &mut TcpStream) -> Option<Req> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut tmp)).await.ok()?.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > 65536 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let mut clen = 0usize;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                clen = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let clen = clen.min(1024 * 1024);
    let mut body = buf[head_end..].to_vec();
    while body.len() < clen {
        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut tmp)).await.ok()?.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    let (path, qs) = target.split_once('?').unwrap_or((&target, ""));
    let query = url::form_urlencoded::parse(qs.as_bytes()).into_owned().collect();
    Some(Req { method, path: path.to_string(), query, body })
}

async fn send(sock: &mut TcpStream, code: u16, ctype: &str, cache: &str, body: &[u8]) {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: {}\r\nConnection: close\r\n\r\n",
        code,
        reason,
        ctype,
        body.len(),
        cache
    )
    .into_bytes();
    out.extend_from_slice(body);
    let _ = sock.write_all(&out).await;
    let _ = sock.shutdown().await;
}

const JSON: &str = "application/json; charset=utf-8";

async fn respond(sock: &mut TcpStream, code: u16, body: &[u8]) {
    send(sock, code, JSON, "no-store", body).await
}

fn jerr(msg: &str) -> String {
    json!({ "error": msg }).to_string()
}

#[derive(Deserialize)]
struct PlayBody {
    #[serde(default)]
    ctx: String,
    #[serde(default)]
    uri: String,
    #[serde(default)]
    index: usize,
    #[serde(default)]
    shuffle: bool,
    #[serde(default)]
    items: Vec<Item>,
}

fn shuffle<T>(v: &mut [T]) {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    for i in (1..v.len()).rev() {
        let j = rng.gen_range(0..=i);
        v.swap(i, j);
    }
}

async fn handle(app: Arc<App>, mut sock: TcpStream) {
    let _ = sock.set_nodelay(true);
    let Some(r) = read_req(&mut sock).await else { return };
    if let Some(hexid) = r.path.strip_prefix("/img/") {
        match app.image(hexid).await {
            Ok(b) => send(&mut sock, 200, "image/jpeg", "public, max-age=31536000, immutable", &b).await,
            Err(e) => respond(&mut sock, 404, jerr(&e).as_bytes()).await,
        }
        return;
    }
    let q = |k: &str| r.query.get(k).cloned().unwrap_or_default();
    let t = Instant::now();
    let api: Option<Result<Value, String>> = match r.path.as_str() {
        "/api/playlists" => Some(app.api_playlists().await),
        "/api/playlist" => Some(app.api_playlist(&q("id")).await),
        "/api/liked" => Some(app.api_liked().await),
        "/api/album" => Some(app.api_album(&q("id")).await),
        "/api/search" => Some(app.api_search(&q("q")).await),
        _ => None,
    };
    if let Some(res) = api {
        match res {
            Ok(v) => {
                let body = v.to_string();
                if ms(t) > 150 {
                    info!("{} {} ms ({} KB)", r.path, ms(t), body.len() / 1024);
                }
                respond(&mut sock, 200, body.as_bytes()).await
            }
            Err(e) => {
                warn!("{} failed: {}", r.path, e);
                respond(&mut sock, 502, jerr(&e).as_bytes()).await
            }
        }
        return;
    }
    match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            send(&mut sock, 200, "text/html; charset=utf-8", "no-cache", INDEX_HTML.as_bytes()).await
        }
        ("GET", "/api/status") => respond(&mut sock, 200, app.status_json().as_bytes()).await,
        ("POST", "/api/play") => {
            let Ok(b) = serde_json::from_slice::<PlayBody>(&r.body) else {
                respond(&mut sock, 400, jerr("invalid body").as_bytes()).await;
                return;
            };
            let mut items = if !b.ctx.is_empty() {
                match app.ctx_items(&b.ctx).await {
                    Ok(v) => v,
                    Err(e) => {
                        respond(&mut sock, 502, jerr(&e).as_bytes()).await;
                        return;
                    }
                }
            } else {
                b.items
            };
            if items.is_empty() {
                respond(&mut sock, 400, jerr("empty list").as_bytes()).await;
                return;
            }
            let mut idx = if b.uri.is_empty() {
                b.index.min(items.len() - 1)
            } else {
                items.iter().position(|i| i.uri == b.uri).unwrap_or(0)
            };
            if b.shuffle {
                if b.uri.is_empty() {
                    shuffle(&mut items);
                } else {
                    let first = items.remove(idx);
                    shuffle(&mut items);
                    items.insert(0, first);
                }
                idx = 0;
            }
            {
                let mut qq = app.q.lock().unwrap();
                qq.items = items;
                qq.idx = idx;
                qq.ctx = b.ctx;
            }
            if let Err(e) = app.ensure_session().await {
                respond(&mut sock, 502, jerr(&e).as_bytes()).await;
                return;
            }
            app.load_current(true);
            respond(&mut sock, 200, app.status_json().as_bytes()).await
        }
        (_, "/api/pause") => {
            app.player.pause();
            respond(&mut sock, 200, app.status_json().as_bytes()).await
        }
        (_, "/api/resume") => {
            let empty = app.q.lock().unwrap().items.is_empty();
            if !empty {
                let _ = app.ensure_session().await;
                app.player.play();
            }
            respond(&mut sock, 200, app.status_json().as_bytes()).await
        }
        (_, "/api/next") => {
            app.skip(1);
            respond(&mut sock, 200, app.status_json().as_bytes()).await
        }
        (_, "/api/prev") => {
            let pos = {
                let q = app.q.lock().unwrap();
                q.pos_ms as u64 + q.pos_at.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0)
            };
            if pos > 4000 || !app.skip(-1) {
                audio_flush();
                app.player.seek(0);
            }
            respond(&mut sock, 200, app.status_json().as_bytes()).await
        }
        (_, "/api/seek") => {
            let ms = r.query.get("ms").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
            audio_flush();
            app.player.seek(ms);
            respond(&mut sock, 200, app.status_json().as_bytes()).await
        }
        (_, "/api/jump") => {
            let i = r.query.get("i").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            let ok = {
                let mut q = app.q.lock().unwrap();
                if i < q.items.len() {
                    q.idx = i;
                    true
                } else {
                    false
                }
            };
            if ok {
                app.load_current(true);
            }
            respond(&mut sock, 200, app.status_json().as_bytes()).await
        }
        (_, "/api/queue") => {
            let body = {
                let q = app.q.lock().unwrap();
                json!({ "idx": q.idx, "tracks": q.items }).to_string()
            };
            respond(&mut sock, 200, body.as_bytes()).await
        }
        _ => respond(&mut sock, 404, jerr("not found").as_bytes()).await,
    }
}

// ---------------------------------------------------------------- entry
fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// dir: data dir holding credentials.json (reusable creds) and caches. port: HTTP port.
#[no_mangle]
pub extern "C" fn spotipad_main(dir: *const c_char, port: c_int) -> c_int {
    let _ = env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .filter_module("rustls", log::LevelFilter::Warn)
        .filter_module("symphonia_format_ogg", log::LevelFilter::Warn)
        .filter_module("librespot_core::cdn_url", log::LevelFilter::Error)
        .parse_default_env()
        .format_timestamp_secs()
        .try_init();
    let dir = PathBuf::from(cstr(dir));
    let _ = std::fs::create_dir_all(dir.join("img"));
    let _ = rustls::crypto::CryptoProvider::install_default(rustls_rustcrypto::provider());
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        // Threads must never exit: on armv7/iOS 9 the parking_lot thread-local destructor
        // faults (EXC_ARM_DA_ALIGN) when a tokio blocking-pool thread retires after idling.
        .thread_keep_alive(Duration::from_secs(365 * 24 * 3600))
        .enable_all()
        .build() {
        Ok(r) => r,
        Err(_) => return -10,
    };
    rt.block_on(async move {
        let cache = match Cache::new(Some(&dir), None, None, None) {
            Ok(c) => c,
            Err(e) => {
                error!("cache: {}", e);
                return -11;
            }
        };
        let store = App::load_store(&dir);
        // HTTP comes up before the Spotify connection so cached views render instantly.
        let listener = match TcpListener::bind(("0.0.0.0", port as u16)).await {
            Ok(l) => l,
            Err(e) => {
                error!("bind :{}: {}", port, e);
                return -13;
            }
        };
        let cfg = SessionConfig::default();
        let session = Session::new(cfg.clone(), Some(cache.clone()));
        let pcfg = PlayerConfig { bitrate: Bitrate::Bitrate160, ..PlayerConfig::default() };
        let player = Player::new(pcfg, session.clone(), Box::new(NoOpVolume), || Box::new(AqSink));
        let app = Arc::new(App {
            dir: dir.clone(),
            session: Mutex::new(session.clone()),
            player,
            cache: cache.clone(),
            cfg,
            q: Mutex::new(Queue::default()),
            store: Mutex::new(store),
            dirty: AtomicBool::new(false),
            root_at: Mutex::new(None),
            liked_at: Mutex::new(None),
            inflight: Mutex::new(HashSet::new()),
            searches: Mutex::new(HashMap::new()),
        });
        {
            let app = app.clone();
            tokio::spawn(async move {
                info!("http on :{}", port);
                loop {
                    if let Ok((sock, _)) = listener.accept().await {
                        tokio::spawn(handle(app.clone(), sock));
                    }
                }
            });
        }
        let t = Instant::now();
        let mut wait = 2;
        loop {
            let Some(creds) = cache.credentials() else {
                error!("no credentials.json in {}", dir.display());
                return -12;
            };
            match session.connect(creds, true).await {
                Ok(()) => break,
                Err(e) => {
                    error!("connect failed: {} (retry in {}s)", e, wait);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    wait = (wait * 2).min(60);
                }
            }
        }
        info!("connected as {} in {} ms", session.username(), ms(t));
        tokio::spawn(event_loop(app.clone()));
        {
            let app = app.clone();
            tokio::spawn(async move { app.warmup().await });
        }
        housekeeping(app).await;
        0
    })
}

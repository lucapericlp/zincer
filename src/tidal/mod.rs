mod model;
mod response;

use std::path::PathBuf;

use async_trait::async_trait;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use model::{TidalMediaResponse, TidalMediaResponseSingle, TidalOAuthDeviceRes};
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::json;
use tracing::{info, warn};

use self::model::{TidalPageResponse, TidalPlaylistResponse, TidalSongItemResponse};
use crate::ConfigArgs;
use crate::music_api::{
    MusicApi, MusicApiType, OAuthRefreshToken, OAuthReqToken, OAuthToken, PLAYLIST_DESC, Playlist,
    Playlists, Song, Songs,
};
use crate::tidal::model::{TidalPlaylistCreateResponse, TidalSearchResponse};
use crate::utils::{
    debug_response_bytes, http_error_with_body, log_request, parse_response_json,
};

pub struct TidalApi {
    client: reqwest::Client,
    config: ConfigArgs,
    user_id: String,
    country_code: String,
}

#[derive(Debug)]
enum HttpMethod<'a> {
    Get(&'a serde_json::Value),
    Post(&'a serde_json::Value),
    Put(&'a serde_json::Value),
}

impl TidalApi {
    const API_URL: &'static str = "https://api.tidal.com";
    const API_V2_URL: &'static str = "https://openapi.tidal.com/v2";

    const AUTH_URL: &'static str = "https://auth.tidal.com/v1/oauth2/device_authorization";
    const TOKEN_URL: &'static str = "https://auth.tidal.com/v1/oauth2/token";
    const SCOPE: &'static str = "r_usr w_usr w_sub";
    const RES_DEBUG_FILENAME: &'static str = MusicApiType::Tidal.short_name();
    const MAX_RETRIES: u32 = 5;
    const ADD_CHUNK_SIZE: usize = 100;

    pub async fn new(
        client_id: &str,
        client_secret: &str,
        oauth_token_path: PathBuf,
        clear_cache: bool,
        config: ConfigArgs,
    ) -> Result<Self> {
        let token = if !oauth_token_path.exists() || clear_cache {
            info!("requesting new token");
            Self::request_token(client_id, client_secret, &config).await?
        } else {
            info!("refreshing token");
            match Self::refresh_token(client_id, client_secret, &oauth_token_path, &config).await {
                Ok(token) => token,
                Err(err) => {
                    warn!("failed to refresh TIDAL token, requesting a new one: {err}");
                    Self::request_token(client_id, client_secret, &config).await?
                }
            }
        };
        // Write new token
        let mut file = std::fs::File::create(&oauth_token_path)?;
        serde_json::to_writer(&mut file, &token)?;

        let mut headers = HeaderMap::new();
        headers.insert(
            "Authorization",
            format!("Bearer {}", token.access_token).parse()?,
        );
        headers.insert("Content-Type", "application/vnd.tidal.v1+json".parse()?);

        let mut client = reqwest::Client::builder()
            .cookie_store(true)
            .default_headers(headers);
        if let Some(proxy) = &config.proxy {
            client = client
                .proxy(reqwest::Proxy::all(proxy)?)
                .danger_accept_invalid_certs(true);
        }
        let client = client.build()?;

        let url = format!("{}/users/me", Self::API_V2_URL);
        let res = client.get(&url).send().await?;
        let status = res.status();
        if !status.is_success() {
            let body = debug_response_bytes(&config, res, Self::RES_DEBUG_FILENAME).await?;
            return Err(http_error_with_body(status, &body));
        }
        let body = debug_response_bytes(&config, res, Self::RES_DEBUG_FILENAME).await?;
        let me_res: TidalMediaResponseSingle = parse_response_json(&body, Self::RES_DEBUG_FILENAME)?;
        let country_code = me_res.data.attributes.country.unwrap_or("US".into());

        Ok(Self {
            client,
            config,
            user_id: me_res.data.id,
            country_code,
        })
    }

    async fn request_token(
        client_id: &str,
        client_secret: &str,
        config: &ConfigArgs,
    ) -> Result<OAuthToken> {
        let client = reqwest::Client::new();
        let params = json!({
            "client_id": client_id,
            "scope": Self::SCOPE,
        });
        let res = client.post(Self::AUTH_URL).form(&params).send().await?;
        let status = res.status();
        if !status.is_success() {
            let body = debug_response_bytes(config, res, Self::RES_DEBUG_FILENAME).await?;
            return Err(http_error_with_body(status, &body));
        }
        let body = debug_response_bytes(config, res, Self::RES_DEBUG_FILENAME).await?;
        let device_res: TidalOAuthDeviceRes = parse_response_json(&body, Self::RES_DEBUG_FILENAME)?;

        let url = if device_res.verification_uri_complete.starts_with("http://")
            || device_res.verification_uri_complete.starts_with("https://")
        {
            device_res.verification_uri_complete.clone()
        } else {
            format!("https://{}", device_res.verification_uri_complete)
        };

        webbrowser::open(&url)?;
        info!("please authorize the app in your browser: {}", url);

        let auth_token = OAuthReqToken {
            client_id: client_id.to_string(),
            device_code: device_res.device_code.clone(),
            grant_type: "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            scope: Self::SCOPE.to_string(),
        };
        let poll_interval = device_res.interval.unwrap_or(2);
        let expires_at = std::time::Instant::now()
            + std::time::Duration::from_secs(u64::from(device_res.expires_in));

        loop {
            let res = client
                .post(Self::TOKEN_URL)
                .basic_auth(client_id, Some(client_secret))
                .form(&auth_token)
                .send()
                .await?;
            let status = res.status();
            let body = res.text().await?;

            if status.is_success() {
                return Ok(serde_json::from_str(&body)?);
            }

            let error = serde_json::from_str::<TidalOAuthPendingError>(&body).ok();
            match error.as_ref().map(|err| err.error.as_str()) {
                Some("authorization_pending") => {
                    tokio::time::sleep(std::time::Duration::from_secs(poll_interval)).await;
                }
                Some("slow_down") => {
                    tokio::time::sleep(std::time::Duration::from_secs(poll_interval + 1)).await;
                }
                Some("expired_token") => {
                    return Err(eyre!(
                        "TIDAL device authorization expired before completion"
                    ));
                }
                _ => {
                    return Err(eyre!(
                        "Invalid HTTP status: {} while requesting TIDAL token: {}",
                        status,
                        body
                    ));
                }
            }

            if std::time::Instant::now() >= expires_at {
                return Err(eyre!("TIDAL device authorization timed out"));
            }
        }
    }

    async fn refresh_token(
        client_id: &str,
        client_secret: &str,
        oauth_token_path: &PathBuf,
        config: &ConfigArgs,
    ) -> Result<OAuthToken> {
        let client = reqwest::Client::new();
        let reader = std::fs::File::open(oauth_token_path)?;
        let mut oauth_token: OAuthToken = serde_json::from_reader(reader)?;

        let params = json!({
            "client_id": client_id,
            "client_secret": client_secret,
            "grant_type": "refresh_token",
            "refresh_token": &oauth_token.refresh_token,
        });

        let res = client.post(Self::TOKEN_URL).form(&params).send().await?;
        let status = res.status();
        if !status.is_success() {
            let body = debug_response_bytes(config, res, Self::RES_DEBUG_FILENAME).await?;
            return Err(http_error_with_body(status, &body));
        }
        let body = debug_response_bytes(config, res, Self::RES_DEBUG_FILENAME).await?;
        let refresh_token: OAuthRefreshToken = parse_response_json(&body, Self::RES_DEBUG_FILENAME)?;

        oauth_token.access_token = refresh_token.access_token;
        oauth_token.expires_in = refresh_token.expires_in;
        oauth_token.scope = refresh_token.scope;
        Ok(oauth_token)
    }

    async fn paginated_request<T>(
        &self,
        url: &str,
        method: &HttpMethod<'_>,
        limit: usize,
    ) -> Result<TidalPageResponse<T>>
    where
        T: DeserializeOwned + std::fmt::Debug,
    {
        let mut res: TidalPageResponse<T> = self
            .make_request_json(url, method, Some((limit, 0)))
            .await?;
        if res.items.is_empty() {
            return Ok(res);
        }

        let mut offset = limit;
        while offset < res.total_number_of_items {
            let res2: TidalPageResponse<T> = self
                .make_request_json(url, method, Some((limit, offset)))
                .await?;
            if res2.items.is_empty() {
                break;
            }
            res.items.extend(res2.items);
            offset += limit;
        }
        Ok(res)
    }

    async fn make_request_json<T>(
        &self,
        url: &str,
        method: &HttpMethod<'_>,
        lim_off: Option<(usize, usize)>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        for attempt in 0..=Self::MAX_RETRIES {
            let mut request = match method {
                HttpMethod::Get(p) => self.client.get(url).query(p),
                HttpMethod::Post(b) => {
                    log_request(Self::RES_DEBUG_FILENAME, "POST", url, b);
                    self.client.post(url).form(b)
                }
                HttpMethod::Put(b) => {
                    log_request(Self::RES_DEBUG_FILENAME, "PUT", url, b);
                    self.client.put(url).form(b)
                }
            };
            if let Some((limit, offset)) = lim_off {
                request = request.query(&[("limit", limit), ("offset", offset)]);
            }

            let res = request.send().await?;
            let status = res.status();
            if status.is_success() {
                let body = debug_response_bytes(&self.config, res, Self::RES_DEBUG_FILENAME).await?;
                let obj = parse_response_json(&body, Self::RES_DEBUG_FILENAME)?;
                return Ok(obj);
            }

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                Self::wait_before_retry(&res, attempt, "TIDAL rate limit").await?;
                continue;
            }

            let body = debug_response_bytes(&self.config, res, Self::RES_DEBUG_FILENAME).await?;
            if status.is_server_error() && attempt < Self::MAX_RETRIES {
                warn!(
                    "transient TIDAL API error on {} (attempt {}/{}): {}",
                    url,
                    attempt + 1,
                    Self::MAX_RETRIES + 1,
                    String::from_utf8_lossy(&body)
                );
                Self::sleep_before_retry(attempt).await;
                continue;
            }
            return Err(http_error_with_body(status, &body));
        }

        unreachable!("TIDAL request retry loop exhausted unexpectedly")
    }

    async fn wait_before_retry(res: &reqwest::Response, attempt: u32, context: &str) -> Result<()> {
        let delay = res
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| 1 + u64::from(attempt));
        warn!("{context}, retrying in {delay}s");
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        Ok(())
    }

    async fn sleep_before_retry(attempt: u32) {
        let delay = 1_u64 << attempt.min(4);
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
    }

    async fn fetch_playlist_etag(&self, playlist_id: &str) -> Result<reqwest::header::HeaderValue> {
        let url = format!("{}/v1/playlists/{}", Self::API_URL, playlist_id);
        let params = json!({
            "countryCode": self.country_code,
        });

        for attempt in 0..=Self::MAX_RETRIES {
            let res = self.client.get(&url).query(&params).send().await?;
            let status = res.status();
            let etag = res.headers().get("ETag").cloned();

            if status.is_success() {
                let body = debug_response_bytes(&self.config, res, Self::RES_DEBUG_FILENAME).await?;
                let _: IgnoredAny = parse_response_json(&body, Self::RES_DEBUG_FILENAME)?;
                return etag.ok_or(eyre!("No ETag in Tidal Response"));
            }

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                Self::wait_before_retry(&res, attempt, "TIDAL rate limit").await?;
                continue;
            }

            let body = debug_response_bytes(&self.config, res, Self::RES_DEBUG_FILENAME).await?;
            if status.is_server_error() && attempt < Self::MAX_RETRIES {
                warn!(
                    "transient TIDAL playlist ETag error (attempt {}/{}): {}",
                    attempt + 1,
                    Self::MAX_RETRIES + 1,
                    String::from_utf8_lossy(&body)
                );
                Self::sleep_before_retry(attempt).await;
                continue;
            }
            return Err(http_error_with_body(status, &body));
        }

        unreachable!("TIDAL ETag retry loop exhausted unexpectedly")
    }

    async fn add_song_chunk_to_playlist(
        &self,
        playlist_id: &str,
        etag: reqwest::header::HeaderValue,
        songs: &[Song],
    ) -> Result<()> {
        let url = format!("{}/v1/playlists/{}/items", Self::API_URL, playlist_id);
        let params = json!({
            "trackIds": songs.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(","),
            "onDuplicate": "FAIL",
            "onArtifactNotFound": "FAIL",
        });
        let mut etag = etag;

        for attempt in 0..=Self::MAX_RETRIES {
            let res = self
                .client
                .post(&url)
                .header("If-None-Match", etag.clone())
                .form(&params)
                .send()
                .await?;
            let status = res.status();

            if status.is_success() {
                let body = debug_response_bytes(&self.config, res, Self::RES_DEBUG_FILENAME).await?;
                let () = parse_response_json(&body, Self::RES_DEBUG_FILENAME)?;
                return Ok(());
            }

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                Self::wait_before_retry(&res, attempt, "TIDAL rate limit").await?;
                continue;
            }

            let body = debug_response_bytes(&self.config, res, Self::RES_DEBUG_FILENAME).await?;
            if status == reqwest::StatusCode::PRECONDITION_FAILED && attempt < Self::MAX_RETRIES {
                warn!(
                    "stale TIDAL playlist ETag while adding songs to {}, refreshing and retrying",
                    playlist_id
                );
                etag = self.fetch_playlist_etag(playlist_id).await?;
                continue;
            }
            if status.is_server_error() && attempt < Self::MAX_RETRIES {
                warn!(
                    "transient TIDAL playlist add error (attempt {}/{}): {}",
                    attempt + 1,
                    Self::MAX_RETRIES + 1,
                    String::from_utf8_lossy(&body)
                );
                Self::sleep_before_retry(attempt).await;
                continue;
            }
            return Err(http_error_with_body(status, &body));
        }

        unreachable!("TIDAL add chunk retry loop exhausted unexpectedly")
    }
}

#[derive(Deserialize, Debug)]
struct TidalOAuthPendingError {
    error: String,
}

#[async_trait]
impl MusicApi for TidalApi {
    fn api_type(&self) -> MusicApiType {
        MusicApiType::Tidal
    }

    fn country_code(&self) -> &str {
        &self.country_code
    }

    async fn create_playlist(&self, name: &str, public: bool) -> Result<Playlist> {
        info!(
            "creating TIDAL playlist \"{}\" (name length: {} chars, {} bytes)",
            name,
            name.chars().count(),
            name.len()
        );
        let url = format!(
            "{}/v2/my-collection/playlists/folders/create-playlist",
            Self::API_URL
        );
        let params = json!({
            "name": name,
            "description": PLAYLIST_DESC,
            "public": public,
            "folderId": "root"
        });
        let res: TidalPlaylistCreateResponse = self
            .make_request_json(&url, &HttpMethod::Put(&params), Some((5, 0)))
            .await?;

        Ok(Playlist {
            id: res.data.uuid,
            name: name.to_string(),
            songs: vec![],
        })
    }

    async fn get_playlists_info(&self) -> Result<Vec<Playlist>> {
        let url = format!("{}/v1/users/{}/playlists", Self::API_URL, self.user_id);
        let params = json!({
            "countryCode": self.country_code,
        });
        let res: TidalPageResponse<TidalPlaylistResponse> = self
            .paginated_request(&url, &HttpMethod::Get(&params), 100)
            .await?;
        let playlists: Playlists = res.try_into()?;
        Ok(playlists.0)
    }

    async fn get_playlist_songs(&self, id: &str) -> Result<Vec<Song>> {
        let url = format!("{}/v1/playlists/{}/items", Self::API_URL, id);
        let params = json!({
            "countryCode": self.country_code,
        });
        // NOTE: a limit > 100 triggers a 400 error
        let res: TidalPageResponse<TidalSongItemResponse> = self
            .paginated_request(&url, &HttpMethod::Get(&params), 100)
            .await?;
        let songs: Songs = res.try_into()?;
        Ok(songs.0)
    }

    async fn add_songs_to_playlist(&self, playlist: &mut Playlist, songs: &[Song]) -> Result<()> {
        if songs.is_empty() {
            return Ok(());
        }

        for songs_chunk in songs.chunks(Self::ADD_CHUNK_SIZE) {
            let etag = self.fetch_playlist_etag(&playlist.id).await?;
            self.add_song_chunk_to_playlist(&playlist.id, etag, songs_chunk)
                .await?;
        }

        Ok(())
    }

    async fn remove_songs_from_playlist(
        &self,
        _playlist: &mut Playlist,
        _songs_ids: &[Song],
    ) -> Result<()> {
        todo!()
    }

    async fn delete_playlist(&self, playlist: Playlist) -> Result<()> {
        let url = format!(
            "{}/v2/my-collection/playlists/folders/remove",
            Self::API_URL
        );
        let params = json!({
            "trns": format!("trn:playlist:{}", playlist.id),
        });
        let _: IgnoredAny = self
            .make_request_json(&url, &HttpMethod::Put(&params), None)
            .await?;
        Ok(())
    }

    async fn search_song(&self, song: &Song) -> Result<Option<Song>> {
        if let Some(isrc) = &song.isrc {
            let url = format!("{}/tracks", Self::API_V2_URL);
            let params = json!({
                "countryCode": self.country_code,
                "include": "albums,artists",
                "filter[isrc]": isrc.to_uppercase(),
            });
            let res: TidalMediaResponse = self
                .make_request_json(&url, &HttpMethod::Get(&params), Some((1, 0)))
                .await?;
            if res.data.is_empty() {
                return Ok(None);
            }
            let mut res_songs: Songs = res.try_into()?;
            if res_songs.0.is_empty() {
                return Ok(None);
            }
            return Ok(Some(res_songs.0.remove(0)));
        }

        let url = format!("{}/v1/search", Self::API_URL);
        let mut queries = song.build_queries();

        while let Some(query) = queries.pop() {
            let params = json!({
                "countryCode": self.country_code,
                "query": query,
                "type": "TRACKS",
            });
            let res: TidalSearchResponse = self
                .make_request_json(&url, &HttpMethod::Get(&params), Some((3, 0)))
                .await?;
            let res_songs: Songs = res.try_into()?;
            // iterate over top 3 results
            for res_song in res_songs.0.into_iter().take(3) {
                if song.compare(&res_song) {
                    return Ok(Some(res_song));
                }
            }
        }
        Ok(None)
    }

    async fn add_likes(&self, songs: &[Song]) -> Result<()> {
        if songs.is_empty() {
            return Ok(());
        }

        let url = format!(
            "{}/v1/users/{}/favorites/tracks",
            Self::API_URL,
            self.user_id
        );
        let tracks = songs.iter().map(|s| s.id.as_str()).collect::<Vec<_>>();

        // NOTE: we get error 500 if we like too much songs at once
        for tracks_chunk in tracks.chunks(100) {
            let params = json!({
                "countryCode": self.country_code,
                "trackIds": tracks_chunk.join(","),
                "onArtifactNotFound": "FAIL",
            });
            let () = self
                .make_request_json(&url, &HttpMethod::Post(&params), None)
                .await?;
        }
        Ok(())
    }

    async fn get_likes(&self) -> Result<Vec<Song>> {
        let url = format!(
            "{}/v1/users/{}/favorites/tracks",
            Self::API_URL,
            self.user_id
        );
        let params = json!({
            "countryCode": self.country_code,
        });
        let res: TidalPageResponse<TidalSongItemResponse> = self
            .paginated_request(&url, &HttpMethod::Get(&params), 1000)
            .await?;
        let songs: Songs = res.try_into()?;
        Ok(songs.0)
    }
}

use syntra_api::{DeviceProfile, PeerAvatar};
use syntra_proto::{MAX_PROFILE_CHUNK_SIZE, ProtoEvent};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_PARTIALS: usize = 8;
const TTL: Duration = Duration::from_secs(5);
const MAX_CACHE_BYTES: u64 = 512 * 1024;

pub(crate) fn encode_profile(
    request_id: u64,
    profile: &DeviceProfile,
) -> Result<Vec<ProtoEvent>, String> {
    profile.validate().map_err(str::to_owned)?;
    let (width, height, rgba) = profile
        .avatar
        .as_ref()
        .map(|a| (a.width, a.height, a.rgba.as_slice()))
        .unwrap_or((0, 0, &[]));
    let total_len = rgba.len() as u32;
    let chunks = rgba.len().div_ceil(MAX_PROFILE_CHUNK_SIZE) as u32;
    let start = ProtoEvent::ProfileStart {
        request_id,
        width,
        height,
        total_len,
        chunks,
        display_name: profile.display_name.clone(),
    };
    start.clone().encode().map_err(|e| e.to_string())?;
    let mut result = Vec::with_capacity(chunks as usize + 1);
    result.push(start);
    result.extend(
        rgba.chunks(MAX_PROFILE_CHUNK_SIZE)
            .enumerate()
            .map(|(index, data)| ProtoEvent::ProfileChunk {
                request_id,
                index: index as u32,
                data: data.to_vec(),
            }),
    );
    Ok(result)
}

struct Partial {
    request_id: u64,
    width: u32,
    height: u32,
    chunks: Vec<Option<Vec<u8>>>,
    name: String,
    touched: Instant,
}

#[derive(Default)]
pub(crate) struct Reassembler {
    peers: HashMap<String, Partial>,
}

impl Reassembler {
    pub(crate) fn start(
        &mut self,
        fingerprint: &str,
        request_id: u64,
        width: u32,
        height: u32,
        total_len: u32,
        chunks: u32,
        display_name: String,
    ) -> Result<Option<DeviceProfile>, String> {
        self.expire(Instant::now());
        if request_id == 0 || display_name.len() > 128 || width > 128 || height > 128 {
            return Err("invalid profile metadata".into());
        }
        if total_len == 0 {
            if width != 0 || height != 0 || chunks != 0 {
                return Err("invalid empty avatar".into());
            }
            self.peers.remove(fingerprint);
            return Ok(Some(DeviceProfile {
                display_name,
                avatar: None,
            }));
        }
        if width == 0
            || height == 0
            || width * height * 4 != total_len
            || chunks as usize != (total_len as usize).div_ceil(MAX_PROFILE_CHUNK_SIZE)
        {
            return Err("invalid avatar dimensions or chunk count".into());
        }
        if let Some(old) = self.peers.get(fingerprint) {
            if old.request_id == request_id {
                return if old.width == width && old.height == height && old.name == display_name {
                    Ok(None)
                } else {
                    Err("conflicting profile start".into())
                };
            }
        } else if self.peers.len() >= MAX_PARTIALS {
            return Err("too many partial profiles".into());
        }
        // At most 8 * 128 * 128 * 4 bytes can be retained across all peers.
        self.peers.insert(
            fingerprint.to_owned(),
            Partial {
                request_id,
                width,
                height,
                chunks: vec![None; chunks as usize],
                name: display_name,
                touched: Instant::now(),
            },
        );
        Ok(None)
    }

    pub(crate) fn chunk(
        &mut self,
        fingerprint: &str,
        request_id: u64,
        index: u32,
        data: Vec<u8>,
    ) -> Result<Option<DeviceProfile>, String> {
        self.expire(Instant::now());
        let partial = self
            .peers
            .get_mut(fingerprint)
            .ok_or("unknown profile transfer")?;
        if partial.request_id != request_id {
            return Err("profile generation mismatch".into());
        }
        let total = (partial.width * partial.height * 4) as usize;
        let offset = (index as usize)
            .checked_mul(MAX_PROFILE_CHUNK_SIZE)
            .ok_or("invalid profile index")?;
        if offset >= total || data.len() != MAX_PROFILE_CHUNK_SIZE.min(total - offset) {
            return Err("invalid profile chunk size".into());
        }
        let slot = partial
            .chunks
            .get_mut(index as usize)
            .ok_or("invalid profile chunk index")?;
        if let Some(existing) = slot {
            return if existing == &data {
                Ok(None)
            } else {
                Err("conflicting profile chunk".into())
            };
        }
        *slot = Some(data);
        if partial.chunks.iter().any(Option::is_none) {
            return Ok(None);
        }
        let partial = self
            .peers
            .remove(fingerprint)
            .expect("validated partial exists");
        let rgba = partial.chunks.into_iter().flatten().flatten().collect();
        Ok(Some(DeviceProfile {
            display_name: partial.name,
            avatar: Some(PeerAvatar {
                width: partial.width,
                height: partial.height,
                rgba,
            }),
        }))
    }

    pub(crate) fn expire(&mut self, now: Instant) {
        self.peers
            .retain(|_, partial| now.saturating_duration_since(partial.touched) < TTL);
    }
    pub(crate) fn remove_peer(&mut self, fingerprint: &str) {
        self.peers.remove(fingerprint);
    }
}

fn cache_path(config_path: &Path, fingerprint: &str) -> PathBuf {
    let key = Sha256::digest(fingerprint.as_bytes());
    let name = key
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("device-profiles")
        .join(format!("{name}.json"))
}

pub(crate) async fn load_cached(
    config_path: &Path,
    fingerprint: &str,
) -> io::Result<Option<DeviceProfile>> {
    let path = cache_path(config_path, fingerprint);
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.len() > MAX_CACHE_BYTES {
        return Err(io::Error::other("invalid profile cache size"));
    }
    let profile: DeviceProfile =
        serde_json::from_slice(&tokio::fs::read(path).await?).map_err(io::Error::other)?;
    profile.validate().map_err(io::Error::other)?;
    Ok(Some(profile))
}

pub(crate) async fn save_cached(
    config_path: &Path,
    fingerprint: &str,
    profile: &DeviceProfile,
) -> io::Result<()> {
    profile.validate().map_err(io::Error::other)?;
    let path = cache_path(config_path, fingerprint);
    tokio::fs::create_dir_all(path.parent().expect("profile cache parent")).await?;
    let temporary = path.with_extension("tmp");
    let bytes = serde_json::to_vec(profile).map_err(io::Error::other)?;
    tokio::fs::write(&temporary, bytes).await?;
    if let Err(error) = tokio::fs::rename(&temporary, &path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reordered_duplicates_preserve_avatar_and_reject_conflicts() {
        let profile = DeviceProfile {
            display_name: "HP".into(),
            avatar: Some(PeerAvatar {
                width: 128,
                height: 128,
                rgba: vec![173; 128 * 128 * 4],
            }),
        };
        let frames = encode_profile(7, &profile)
            .unwrap()
            .into_iter()
            .map(|frame| {
                let encoded = frame.encode().unwrap();
                assert!(encoded.len() <= 1200);
                ProtoEvent::decode(&encoded).unwrap()
            })
            .collect::<Vec<_>>();
        let mut receiver = Reassembler::default();
        let ProtoEvent::ProfileStart {
            request_id,
            width,
            height,
            total_len,
            chunks,
            display_name,
        } = frames[0].clone()
        else {
            panic!()
        };
        receiver
            .start(
                "cert",
                request_id,
                width,
                height,
                total_len,
                chunks,
                display_name.clone(),
            )
            .unwrap();
        let ProtoEvent::ProfileChunk { index, data, .. } = frames[1].clone() else {
            panic!()
        };
        receiver
            .chunk("cert", request_id, index, data.clone())
            .unwrap();
        receiver
            .start(
                "cert",
                request_id,
                width,
                height,
                total_len,
                chunks,
                display_name,
            )
            .unwrap();
        receiver
            .chunk("cert", request_id, index, data.clone())
            .unwrap();
        let mut conflicting = data;
        conflicting[0] ^= 1;
        assert!(
            receiver
                .chunk("cert", request_id, index, conflicting)
                .is_err()
        );
        let mut result = None;
        for frame in frames.into_iter().skip(2).rev() {
            if let ProtoEvent::ProfileChunk {
                request_id,
                index,
                data,
            } = frame
            {
                result = receiver
                    .chunk("cert", request_id, index, data)
                    .unwrap()
                    .or(result);
            }
        }
        assert_eq!(result, Some(profile));
    }
    #[test]
    fn invalid_dimensions_and_partial_budget_are_bounded() {
        let mut r = Reassembler::default();
        assert!(
            r.start("x", 1, u32::MAX, 128, u32::MAX, u32::MAX, "x".into())
                .is_err()
        );
        for i in 0..MAX_PARTIALS {
            r.start(&i.to_string(), 1, 1, 1, 4, 1, "x".into()).unwrap();
        }
        assert!(r.start("overflow", 1, 1, 1, 4, 1, "x".into()).is_err());
        r.expire(Instant::now() + TTL);
        assert!(r.start("fresh", 2, 1, 1, 4, 1, "x".into()).is_ok());
    }
}

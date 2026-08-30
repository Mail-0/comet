use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpui::{Image, ImageFormat, ImageSource};
use keiki_model::{AvatarState, AvatarTheme};
use zeron_proto::ChatIndicator;

use crate::theme::Theme;

const MAX_ENTRIES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AvatarKey {
    pub agent_id: String,
    pub state: AvatarState,
    pub theme: AvatarTheme,
    pub bucket: u32,
}

impl AvatarKey {
    pub fn new(
        agent_id: impl Into<String>,
        state: AvatarState,
        theme: AvatarTheme,
        bucket: u32,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            state,
            theme,
            bucket,
        }
    }
}

#[derive(Clone)]
pub enum AvatarSnapshot {
    Loaded(Arc<Image>),
    Loading,
    Error { retry_in: Duration },
}

enum CacheEntry {
    Loaded {
        image: Arc<Image>,
        last_used: u64,
    },
    Loading {
        attempts: u32,
        last_used: u64,
    },
    Error {
        attempts: u32,
        at: Instant,
        last_used: u64,
    },
}

#[derive(Default)]
struct ImageCache {
    map: HashMap<AvatarKey, CacheEntry>,
    tick: u64,
    generation: u64,
    pending_free: Vec<Arc<Image>>,
}

impl ImageCache {
    fn next_tick(&mut self) -> u64 {
        self.tick = self.tick.saturating_add(1);
        self.tick
    }

    fn insert_loaded(&mut self, key: AvatarKey, image: Arc<Image>) {
        let tick = self.next_tick();
        if let Some(CacheEntry::Loaded { image, .. }) = self.map.insert(
            key.clone(),
            CacheEntry::Loaded {
                image,
                last_used: tick,
            },
        ) {
            self.pending_free.push(image);
        }
        self.evict_overflow(&key);
    }

    fn evict_overflow(&mut self, protected: &AvatarKey) {
        while self.map.len() > MAX_ENTRIES {
            let Some(oldest) = self
                .map
                .iter()
                .filter(|(key, _)| *key != protected)
                .map(|(key, entry)| {
                    let last_used = match entry {
                        CacheEntry::Loaded { last_used, .. }
                        | CacheEntry::Loading { last_used, .. }
                        | CacheEntry::Error { last_used, .. } => *last_used,
                    };
                    (last_used, key.clone())
                })
                .min_by_key(|(last_used, _)| *last_used)
            else {
                break;
            };
            if let Some(CacheEntry::Loaded { image, .. }) = self.map.remove(&oldest.1) {
                self.pending_free.push(image);
            }
        }
    }
}

fn cache() -> &'static Mutex<ImageCache> {
    static CACHE: OnceLock<Mutex<ImageCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ImageCache::default()))
}

fn lock_cache() -> std::sync::MutexGuard<'static, ImageCache> {
    cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn retry_delay(attempts: u32) -> Duration {
    Duration::from_secs((2u64 << attempts.min(3)).min(15))
}

pub fn avatar_state(indicator: ChatIndicator) -> AvatarState {
    match indicator {
        ChatIndicator::Working => AvatarState::Running,
        ChatIndicator::AwaitingInput => AvatarState::Thinking,
        ChatIndicator::Errored => AvatarState::Error,
        ChatIndicator::Completed | ChatIndicator::Idle => AvatarState::Idle,
    }
}

pub fn group_avatar_state(states: impl Iterator<Item = AvatarState>) -> AvatarState {
    states
        .max_by_key(|state| match state {
            AvatarState::Idle => 0,
            AvatarState::Thinking => 1,
            AvatarState::Running => 2,
            AvatarState::Error => 3,
        })
        .unwrap_or(AvatarState::Idle)
}

pub fn avatar_theme(theme: &Theme) -> AvatarTheme {
    if theme.appearance.is_dark() {
        AvatarTheme::Dark
    } else {
        AvatarTheme::Light
    }
}

pub fn snapshot(key: &AvatarKey) -> AvatarSnapshot {
    let mut cache = lock_cache();
    let tick = cache.next_tick();
    match cache.map.get_mut(key) {
        Some(CacheEntry::Loaded { image, last_used }) => {
            *last_used = tick;
            AvatarSnapshot::Loaded(image.clone())
        }
        Some(CacheEntry::Loading { last_used, .. }) => {
            *last_used = tick;
            AvatarSnapshot::Loading
        }
        Some(CacheEntry::Error {
            attempts,
            at,
            last_used,
        }) => {
            *last_used = tick;
            AvatarSnapshot::Error {
                retry_in: retry_delay(attempts.saturating_sub(1)).saturating_sub(at.elapsed()),
            }
        }
        None => AvatarSnapshot::Loading,
    }
}

pub fn begin_load(key: &AvatarKey) -> bool {
    let mut cache = lock_cache();
    let tick = cache.next_tick();
    match cache.map.entry(key.clone()) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(CacheEntry::Loading {
                attempts: 0,
                last_used: tick,
            });
            true
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => match entry.get_mut() {
            CacheEntry::Loading { last_used, .. } => {
                *last_used = tick;
                false
            }
            CacheEntry::Loaded { last_used, .. } => {
                *last_used = tick;
                false
            }
            CacheEntry::Error {
                attempts,
                at,
                last_used,
            } if at.elapsed() >= retry_delay(attempts.saturating_sub(1)) => {
                let attempts = *attempts;
                *entry.get_mut() = CacheEntry::Loading {
                    attempts,
                    last_used: tick,
                };
                true
            }
            CacheEntry::Error { last_used, .. } => {
                *last_used = tick;
                false
            }
        },
    }
}

pub fn store_loaded(key: AvatarKey, bytes: Vec<u8>) {
    lock_cache().insert_loaded(key, Arc::new(Image::from_bytes(ImageFormat::Gif, bytes)));
}

pub fn generation() -> u64 {
    lock_cache().generation
}

pub fn store_error(key: &AvatarKey) {
    let mut cache = lock_cache();
    let tick = cache.next_tick();
    let attempts = match cache.map.get(key) {
        Some(CacheEntry::Loading { attempts, .. }) => attempts + 1,
        Some(CacheEntry::Error { attempts, .. }) => *attempts,
        _ => 1,
    };
    cache.map.insert(
        key.clone(),
        CacheEntry::Error {
            attempts,
            at: Instant::now(),
            last_used: tick,
        },
    );
}

pub fn clear() {
    let mut cache = lock_cache();
    cache.generation = cache.generation.wrapping_add(1);
    let entries = std::mem::take(&mut cache.map);
    for entry in entries.into_values() {
        if let CacheEntry::Loaded { image, .. } = entry {
            cache.pending_free.push(image);
        }
    }
}

pub fn flush_evicted(mut window: Option<&mut gpui::Window>, cx: &mut gpui::App) {
    let evicted = std::mem::take(&mut lock_cache().pending_free);
    for image in evicted {
        ImageSource::Image(image).evict(window.as_deref_mut(), cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_chat_indicators_to_avatar_states() {
        assert_eq!(avatar_state(ChatIndicator::Working), AvatarState::Running);
        assert_eq!(
            avatar_state(ChatIndicator::AwaitingInput),
            AvatarState::Thinking
        );
        assert_eq!(avatar_state(ChatIndicator::Errored), AvatarState::Error);
        assert_eq!(avatar_state(ChatIndicator::Completed), AvatarState::Idle);
        assert_eq!(avatar_state(ChatIndicator::Idle), AvatarState::Idle);
    }

    #[test]
    fn groups_states_by_severity() {
        assert_eq!(
            group_avatar_state(
                [
                    AvatarState::Idle,
                    AvatarState::Thinking,
                    AvatarState::Running
                ]
                .into_iter()
            ),
            AvatarState::Running
        );
        assert_eq!(
            group_avatar_state([AvatarState::Error, AvatarState::Running].into_iter()),
            AvatarState::Error
        );
        assert_eq!(group_avatar_state(std::iter::empty()), AvatarState::Idle);
    }

    #[test]
    fn maps_theme_appearance_to_avatar_ink() {
        assert_eq!(avatar_theme(&Theme::dark()), AvatarTheme::Dark);
        assert_eq!(avatar_theme(&Theme::light()), AvatarTheme::Light);
    }

    #[test]
    fn retry_ladder_is_bounded() {
        assert_eq!(retry_delay(0), Duration::from_secs(2));
        assert_eq!(retry_delay(1), Duration::from_secs(4));
        assert_eq!(retry_delay(2), Duration::from_secs(8));
        assert_eq!(retry_delay(3), Duration::from_secs(15));
        assert_eq!(retry_delay(20), Duration::from_secs(15));
    }
}

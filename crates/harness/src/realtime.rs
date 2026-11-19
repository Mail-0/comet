//! Provider-independent controls for an optional realtime voice runtime.

use std::sync::{Arc, Mutex, atomic::Ordering};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use zeron_proto::voice::*;

type AudioAbort = Arc<Mutex<Option<(u64, CancellationToken)>>>;

pub enum VoiceCommand {
    Probe {
        external: bool,
        reply: oneshot::Sender<Result<VoiceEligibility, VoiceRejection>>,
    },
    Start {
        voice: Option<String>,
        session_id: String,
        generation: u64,
        reply: oneshot::Sender<Result<(), VoiceRejection>>,
    },
    External {
        voice: Option<String>,
        session_id: String,
        generation: u64,
        offer: remote::Sdp,
        reply: oneshot::Sender<Result<remote::Sdp, VoiceRejection>>,
    },
    Mute {
        muted: bool,
        reply: oneshot::Sender<Result<(), VoiceRejection>>,
    },
    Append {
        frame: VoiceFrame,
        reply: oneshot::Sender<Result<(), VoiceRejection>>,
    },
    Stop {
        reply: oneshot::Sender<Result<(), VoiceRejection>>,
    },
}
pub struct RealtimeControls {
    audio_abort: AudioAbort,
    invalidated: Arc<std::sync::atomic::AtomicBool>,
    pub commands: mpsc::Receiver<VoiceCommand>,
    pub events: broadcast::Sender<VoiceEvent>,
}
#[derive(Clone)]
pub struct RealtimeHandle {
    audio_abort: AudioAbort,
    invalidated: Arc<std::sync::atomic::AtomicBool>,
    pub commands: mpsc::Sender<VoiceCommand>,
    pub events: broadcast::Sender<VoiceEvent>,
}
impl RealtimeHandle {
    pub fn invalidated(&self) -> bool {
        self.invalidated.load(Ordering::Acquire)
    }
    /// Immediately closes native capture even if the control actor is waiting on I/O.
    pub fn abort_voice(&self, generation: u64) {
        if let Some((current, cancel)) = self.audio_abort.lock().unwrap().as_ref() {
            if *current == generation {
                cancel.cancel();
            }
        }
    }

    pub async fn probe(&self) -> Result<VoiceEligibility, VoiceRejection> {
        self.probe_mode(false).await
    }
    pub async fn probe_external(&self) -> Result<VoiceEligibility, VoiceRejection> {
        self.probe_mode(true).await
    }
    async fn probe_mode(&self, external: bool) -> Result<VoiceEligibility, VoiceRejection> {
        let (reply, rx) = oneshot::channel();
        tokio::time::timeout(Duration::from_secs(15), async {
            self.commands
                .send(VoiceCommand::Probe { external, reply })
                .await
                .map_err(|_| VoiceRejection::Protocol)?;
            rx.await.map_err(|_| VoiceRejection::Protocol)?
        })
        .await
        .map_err(|_| VoiceRejection::Protocol)?
    }

    pub async fn stop(&self) -> Result<(), VoiceRejection> {
        let (reply, rx) = oneshot::channel();
        tokio::time::timeout(Duration::from_secs(8), async {
            self.commands
                .send(VoiceCommand::Stop { reply })
                .await
                .map_err(|_| VoiceRejection::Protocol)?;
            rx.await.map_err(|_| VoiceRejection::Protocol)?
        })
        .await
        .map_err(|_| VoiceRejection::Protocol)?
    }
    pub async fn mute(&self, muted: bool) -> Result<(), VoiceRejection> {
        let (reply, rx) = oneshot::channel();
        tokio::time::timeout(Duration::from_secs(8), async {
            self.commands
                .send(VoiceCommand::Mute { muted, reply })
                .await
                .map_err(|_| VoiceRejection::Protocol)?;
            rx.await.map_err(|_| VoiceRejection::Protocol)?
        })
        .await
        .map_err(|_| VoiceRejection::Protocol)?
    }
}
pub fn channel() -> (
    RealtimeHandle,
    RealtimeControls,
    broadcast::Receiver<VoiceEvent>,
) {
    let audio_abort = Arc::default();
    let invalidated = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (commands, rx) = mpsc::channel(8);
    let (events, receiver) = broadcast::channel(32);
    (
        RealtimeHandle {
            audio_abort: Arc::clone(&audio_abort),
            invalidated: invalidated.clone(),
            commands,
            events: events.clone(),
        },
        RealtimeControls {
            audio_abort,
            invalidated,
            commands: rx,
            events,
        },
        receiver,
    )
}

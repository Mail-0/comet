//! In-app update flow: on boot the shell asks the release feed whether a newer
//! build than [`zeron_updater::running_version`] is published; if one is (and
//! this installation is one the updater can replace — see
//! [`zeron_updater::InstallTarget`]), the shell blocks the window with a
//! progress modal, downloads and installs the build, then quits so
//! [`zeron_updater::run_pending_relaunch`] can start the new one.
//!
//! Everything that can fail here is non-fatal: a missing feed, a private
//! release, or a package-managed install just leaves the app running the
//! version it booted with.

use std::time::Duration;

use futures::StreamExt as _;
use gpui::{
    AnyElement, AsyncApp, Context, EntityId, IntoElement, Pixels, SharedString, Task, WeakEntity,
    div, prelude::*, px,
};

use crate::popover;
use crate::shell::Shell;
use crate::theme::{Theme, hairline, ink};

/// How long after boot the check runs — the engine bootstrap and the first
/// frames get the network and the foreground thread to themselves.
const CHECK_DELAY: Duration = Duration::from_secs(5);
/// Hold on "Restarting…" so the modal's final state is readable rather than a
/// flash before the window disappears.
const RESTART_HOLD: Duration = Duration::from_millis(600);

/// Where the update has got to. There is no failure phase: a failed update
/// clears the modal and leaves the running build alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Downloading {
        received: u64,
        /// `None` until the response reports a length.
        total: Option<u64>,
    },
    Installing,
    Restarting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFlow {
    /// Version being installed, without the tag's leading `v`.
    pub version: SharedString,
    pub phase: Phase,
}

impl UpdateFlow {
    /// Status line under the title.
    fn status(&self) -> String {
        match &self.phase {
            Phase::Downloading { received, total } => match progress(*received, *total) {
                Some(fraction) => format!("Downloading… {}%", (fraction * 100.0).round() as u32),
                None => format!("Downloading… {}", megabytes(*received)),
            },
            Phase::Installing => "Installing…".to_string(),
            Phase::Restarting => "Restarting Comet…".to_string(),
        }
    }

    /// Bar fill, 0..=1. Download progress while it is known; the later phases
    /// are short and sit at full.
    fn fill(&self) -> f32 {
        match &self.phase {
            Phase::Downloading { received, total } => progress(*received, *total).unwrap_or(0.0),
            Phase::Installing | Phase::Restarting => 1.0,
        }
    }
}

/// Downloaded fraction, `None` while the total is unknown (or zero, which
/// would divide by nothing).
fn progress(received: u64, total: Option<u64>) -> Option<f32> {
    let total = total.filter(|total| *total > 0)?;
    Some((received as f32 / total as f32).clamp(0.0, 1.0))
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

/// Kick off the boot update check. The returned task is held by the shell, so
/// closing the window cancels an in-flight check.
pub fn start(cx: &mut Context<Shell>) -> Task<()> {
    cx.spawn(async move |shell, cx| {
        cx.background_executor().timer(CHECK_DELAY).await;
        if let Err(error) = update(&shell, cx).await {
            tracing::info!(%error, "update skipped");
            shell
                .update(cx, |shell, cx| {
                    shell.set_update_flow(None, cx);
                })
                .ok();
        }
    })
}

async fn update(shell: &WeakEntity<Shell>, cx: &mut AsyncApp) -> anyhow::Result<()> {
    let updater = zeron_updater::Updater::from_env()?;
    let current = zeron_updater::running_version().to_string();
    let available = cx
        .update({
            let updater = updater.clone();
            move |cx| gpui_tokio::Tokio::spawn(cx, async move { updater.check(&current).await })
        })
        .await??;
    let Some(available) = available else {
        tracing::debug!(
            version = zeron_updater::running_version(),
            "already current"
        );
        return Ok(());
    };
    tracing::info!(
        from = zeron_updater::running_version(),
        to = %available.version,
        asset = %available.asset.name,
        "installing update"
    );
    let version = SharedString::from(available.version.clone());
    shell.update(cx, |shell, cx| {
        shell.set_update_flow(
            Some(UpdateFlow {
                version: version.clone(),
                phase: Phase::Downloading {
                    received: 0,
                    total: None,
                },
            }),
            cx,
        );
    })?;

    // Staged beside nothing in particular: the swap copies out of here into the
    // install directory, and the directory is removed when `staging` drops.
    let staging = tempfile::tempdir()?;
    let (progress_tx, mut progress_rx) = futures::channel::mpsc::unbounded();
    let download = cx.update({
        let updater = updater.clone();
        let asset = available.asset.clone();
        let dir = staging.path().to_path_buf();
        move |cx| {
            gpui_tokio::Tokio::spawn(cx, async move {
                updater
                    .download(&asset, &dir, move |received, total| {
                        // The receiver is dropped once the download resolves;
                        // a send after that is expected, not an error.
                        progress_tx.unbounded_send((received, total)).ok();
                    })
                    .await
            })
        }
    });
    while let Some((received, total)) = progress_rx.next().await {
        shell.update(cx, |shell, cx| {
            shell.set_update_phase(Phase::Downloading { received, total }, cx);
        })?;
    }
    let archive = download.await??;

    shell.update(cx, |shell, cx| {
        shell.set_update_phase(Phase::Installing, cx);
    })?;
    let target = available.target.clone();
    cx.background_executor()
        .spawn(async move { target.install(&archive) })
        .await?;

    shell.update(cx, |shell, cx| {
        shell.set_update_phase(Phase::Restarting, cx);
    })?;
    cx.background_executor().timer(RESTART_HOLD).await;
    // The relaunch itself happens after the window loop exits (`run_app`), so
    // the engine has flushed its stores and dropped the single-instance lock
    // before the new build takes them.
    zeron_updater::queue_relaunch(available.target);
    cx.update(|cx| cx.quit());
    Ok(())
}

/// The blocking update modal: spinner, version line, and a progress bar. No
/// dismiss affordance — the app is a few seconds from restarting into the new
/// build, and a half-installed swap must not be interrupted.
pub fn render_modal(
    flow: &UpdateFlow,
    viewport: gpui::Size<Pixels>,
    view: EntityId,
    cx: &mut gpui::App,
) -> AnyElement {
    let theme = Theme::of(cx).clone();
    let fill = flow.fill();
    let card = popover::dialog_card(&theme)
        .gap(px(12.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .child(crate::loaders::gradient_spinner(
                    "update-modal-spinner",
                    &theme,
                    2.5,
                    view,
                    cx,
                ))
                .child(popover::dialog_title(
                    &theme,
                    &format!("Updating to {}", flow.version),
                )),
        )
        .child(popover::dialog_body(&theme, flow.status()))
        .child(
            div()
                .w_full()
                .h(px(4.0))
                .rounded(px(2.0))
                .bg(ink(0.08))
                .border_1()
                .border_color(hairline(0.06))
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(fill))
                        .rounded(px(2.0))
                        .bg(theme.text),
                ),
        )
        .child(popover::dialog_body(
            &theme,
            SharedString::from("Comet will restart when the update is ready."),
        ))
        .into_any_element();
    popover::modal("update-modal", viewport, card)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_needs_a_nonzero_total() {
        assert_eq!(progress(5, Some(10)), Some(0.5));
        assert_eq!(progress(10, Some(10)), Some(1.0));
        // A server that over-reports must not overflow the bar.
        assert_eq!(progress(20, Some(10)), Some(1.0));
        assert_eq!(progress(5, None), None);
        assert_eq!(progress(5, Some(0)), None);
    }

    #[test]
    fn status_falls_back_to_bytes_without_a_total() {
        let flow = |phase| UpdateFlow {
            version: SharedString::from("0.4.0"),
            phase,
        };
        assert_eq!(
            flow(Phase::Downloading {
                received: 5,
                total: Some(10),
            })
            .status(),
            "Downloading… 50%"
        );
        assert_eq!(
            flow(Phase::Downloading {
                received: 1_048_576,
                total: None,
            })
            .status(),
            "Downloading… 1.0 MB"
        );
        assert_eq!(flow(Phase::Installing).status(), "Installing…");
    }

    #[test]
    fn the_bar_fills_once_the_download_is_done() {
        let flow = |phase| UpdateFlow {
            version: SharedString::from("0.4.0"),
            phase,
        };
        assert_eq!(
            flow(Phase::Downloading {
                received: 1,
                total: Some(4),
            })
            .fill(),
            0.25
        );
        // An unknown total leaves the bar empty rather than jumping around.
        assert_eq!(
            flow(Phase::Downloading {
                received: 1,
                total: None,
            })
            .fill(),
            0.0
        );
        assert_eq!(flow(Phase::Installing).fill(), 1.0);
        assert_eq!(flow(Phase::Restarting).fill(), 1.0);
    }
}

/// Text on the Mudi's OLED via `ubus call mcu send_custom_msg`. GL's status screen overwrites
/// it after about 8 s, so Medium and High warnings are re-sent until the next recording.
use log::{error, info, warn};
use rayhunter::analysis::analyzer::EventType;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use std::time::Duration;

use crate::config::{self, UiLevel};
use crate::display::DisplayState;

const REPEAT: Duration = Duration::from_secs(6);

// Each screen is 4 lines of exactly 16 characters, concatenated.
const MONITORING: &str = "RAYHUNTER       ==============  monitoring      started         ";
const PAUSED: &str = "RAYHUNTER       ==============  recording       paused          ";
const LOW: &str = "RAYHUNTER: LOW  ==============  unusual signal, likely harmless ";
const MEDIUM: &str = "!! RAYHUNTER !! ==============  MEDIUM warning  check web UI    ";
const HIGH: &str = "!! RAYHUNTER !! ==============  HIGH: possible  IMSI catcher!   ";
const HIGH_ACTION: &str = "!! RAYHUNTER !! ==============  airplane mode,  leave the area  ";

async fn show(msg: &str) {
    let payload = serde_json::json!({ "msg": msg }).to_string();
    match tokio::process::Command::new("ubus")
        .args(["call", "mcu", "send_custom_msg", &payload])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {}
        Ok(out) => warn!("ubus mcu send_custom_msg failed: {:?}", out.status),
        Err(e) => error!("failed to run ubus: {e}"),
    }
}

pub fn update_ui(
    task_tracker: &TaskTracker,
    config: &config::Config,
    shutdown_token: CancellationToken,
    mut ui_update_rx: mpsc::Receiver<DisplayState>,
) {
    if config.ui_level == UiLevel::Invisible {
        info!("Invisible mode, not spawning UI.");
        task_tracker.spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_token.cancelled() => break,
                    _ = ui_update_rx.recv() => {}
                }
            }
        });
        return;
    }
    task_tracker.spawn(async move {
        // nothing is shown until the first recording has actually started (diag port open)
        let mut state = DisplayState::Paused;
        let mut changed = false;
        let mut tick: u64 = 0;
        loop {
            match state {
                DisplayState::WarningDetected {
                    event_type: EventType::High,
                } => {
                    show(if tick.is_multiple_of(2) {
                        HIGH
                    } else {
                        HIGH_ACTION
                    })
                    .await;
                    tick += 1;
                }
                DisplayState::WarningDetected {
                    event_type: EventType::Medium,
                } => show(MEDIUM).await,
                DisplayState::WarningDetected { .. } if changed => show(LOW).await,
                DisplayState::Recording if changed => show(MONITORING).await,
                DisplayState::Paused if changed => show(PAUSED).await,
                _ => {}
            }
            changed = false;
            tokio::select! {
                _ = shutdown_token.cancelled() => {
                    info!("received UI shutdown");
                    break;
                }
                new_state = ui_update_rx.recv() => match new_state {
                    Some(new_state) => {
                        changed = new_state != state;
                        if changed {
                            info!("OLED state change");
                            tick = 0;
                        }
                        state = new_state;
                    }
                    None => break,
                },
                _ = tokio::time::sleep(REPEAT) => {}
            }
        }
    });
}

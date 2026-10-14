//! macOS reliable paste.
//!
//! Publishes the transcript with `declareTypes:owner:`, which puts a *promise*
//! on the general pasteboard instead of data. When any consumer actually
//! requests the text, AppKit calls `pasteboard:provideDataForType:` on our
//! owner object — that callback is the read receipt. The previous clipboard is
//! restored after successful post-injection provision and a quiet period,
//! guarded by pasteboard `changeCount` and target focus. A receipt does not
//! identify the reader or prove insertion. A timeout retains the transaction
//! for explicit recovery instead of restoring under a queued paste.
//!
//! Threading: publication, delayed chord injection and restoration run on the
//! main thread. The pre-paste delay and delivery wait do not block AppKit's
//! run loop, which must service promised pasteboard data.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use log::{error, info};
use objc2::rc::Retained;
use objc2_app_kit::{NSPasteboard, NSWorkspace};
use objc2_foundation::NSInteger;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

use super::macos_clipboard::{publish, ClipboardSnapshot, HandyPasteProvider};
use super::{evaluate_mac, send_chord, MacWaitDecision, TxState};
use crate::clipboard::send_return_key;
use crate::input::EnigoState;
use crate::settings::{get_settings, AutoSubmitKey, ClipboardHandling, PasteMethod};

struct MacPending {
    id: u64,
    state: Arc<Mutex<TxState>>,
    snapshot: ClipboardSnapshot,
    target_pid: i32,
    change_count: NSInteger,
    provider: Option<Retained<HandyPasteProvider>>,
    auto_submit: bool,
    auto_submit_key: AutoSubmitKey,
    /// ClipboardHandling::CopyToClipboard — instead of restoring, settle by
    /// re-writing the transcript as plain text (without the concealment
    /// markers), so clipboard managers record it and it outlives the promise.
    preserve_transcript: bool,
    /// The transcript, for the `preserve_transcript` re-write at settle time.
    transcript: String,
    settled: bool,
    unconfirmed: bool,
}

fn foreground_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|app| app.processIdentifier())
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// One transaction owns the clipboard. Never force an earlier queued paste to
/// read a newer transcript or its original clipboard by starting another.
static PENDING: Mutex<Option<Arc<Mutex<MacPending>>>> = Mutex::new(None);

/// Finish a delivered transaction, or restore at the user's explicit request.
/// Must run on the main thread. Never infer insertion success from a timeout.
fn settle(
    pending: &Arc<Mutex<MacPending>>,
    app_handle: &AppHandle,
    explicit_restore: bool,
) -> Result<(), String> {
    let mut p = pending
        .lock()
        .map_err(|_| "Paste transaction lock failed")?;
    if p.settled {
        return Ok(());
    }

    let (receipt_seen, ownership_lost) = match p.state.lock() {
        Ok(st) => (
            st.any_receipt_after_injection() && !st.injection_failed && !st.cancelled,
            st.ownership_lost,
        ),
        Err(_) => (false, true),
    };

    let mut still_ours = (explicit_restore || !ownership_lost)
        && NSPasteboard::generalPasteboard().changeCount() == p.change_count;
    if !still_ours {
        info!("[reliable-paste] clipboard changed externally; leaving it untouched");
    } else if !explicit_restore && !p.preserve_transcript && foreground_pid() != Some(p.target_pid)
    {
        return Err("Paste target changed; clipboard retained for explicit recovery".into());
    } else if p.preserve_transcript && !explicit_restore {
        // The user asked for the transcript to stay on the clipboard: replace
        // the concealed promise with plain text so clipboard managers record
        // it and it survives this app exiting.
        app_handle
            .clipboard()
            .write_text(&p.transcript)
            .map_err(|e| e.to_string())?;
        info!("[reliable-paste] left transcript on clipboard as plain text");
    } else {
        if !explicit_restore && !receipt_seen {
            return Err("Paste not confirmed; clipboard retained for explicit recovery".into());
        }
        still_ours = match p
            .snapshot
            .restore_if_owned(&NSPasteboard::generalPasteboard(), p.change_count)
        {
            Ok(restored) => restored,
            Err(failure) => {
                if let Some(count) = failure.cleared_count {
                    p.change_count = count;
                }
                return Err(failure.message);
            }
        };
        if still_ours {
            info!("[reliable-paste] restored previous clipboard");
        } else {
            info!("[reliable-paste] ownership changed while preparing restoration; leaving clipboard untouched");
        }
    }

    // A receipt is data provision, not proof of insertion. At least guard
    // Enter against an external copy and a focus change, and never send it
    // during explicit cancellation/recovery.
    if !explicit_restore
        && still_ours
        && p.auto_submit
        && receipt_seen
        && foreground_pid() == Some(p.target_pid)
    {
        if let Some(enigo_state) = app_handle.try_state::<EnigoState>() {
            if let Ok(mut e) = enigo_state.0.lock() {
                if let Err(error) = send_return_key(&mut e, p.auto_submit_key) {
                    error!("[reliable-paste] auto-submit failed: {error}");
                    let _ = app_handle.emit("paste-error", ());
                }
            }
        }
    }

    // Release the owner; any outstanding promise dies with the pasteboard
    // contents we just replaced (or with the external change).
    p.provider = None;
    p.settled = true;
    let _ = app_handle.emit("paste-resolved", p.id);
    Ok(())
}

fn clear_completed(pending: &Arc<Mutex<MacPending>>) {
    if let Ok(mut slot) = PENDING.lock() {
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, pending))
        {
            *slot = None;
        }
    }
}

pub(crate) fn pending_id() -> Option<u64> {
    let slot = PENDING.lock().ok()?;
    let p = slot.as_ref()?.lock().ok()?;
    (!p.settled && p.unconfirmed).then_some(p.id)
}

pub(crate) fn cancel_pending() {
    if let Ok(slot) = PENDING.lock() {
        if let Some(pending) = slot.as_ref() {
            if let Ok(p) = pending.lock() {
                if let Ok(mut state) = p.state.lock() {
                    state.cancelled = true;
                }
            }
        }
    }
}

/// Explicit user recovery after an unconfirmed paste. The transaction ID
/// prevents an old notification from restoring a newer transaction.
pub(crate) fn restore_pending(app_handle: &AppHandle, transaction_id: u64) -> Result<(), String> {
    let pending = PENDING
        .lock()
        .map_err(|_| "Paste transaction lock failed")?
        .clone();
    if let Some(pending) = pending {
        if pending
            .lock()
            .map_err(|_| "Paste transaction lock failed")?
            .id
            != transaction_id
        {
            return Err("This paste transaction has already finished".into());
        }
        settle(&pending, app_handle, true)?;
        clear_completed(&pending);
    }
    Ok(())
}

fn mark_unconfirmed(pending: &Arc<Mutex<MacPending>>, app_handle: &AppHandle) -> bool {
    if let Ok(mut p) = pending.lock() {
        if p.settled {
            return false;
        }
        p.unconfirmed = true;
        let _ = app_handle.emit("paste-unconfirmed", p.id);
        return true;
    }
    false
}

fn report_unconfirmed(pending: &Arc<Mutex<MacPending>>, app_handle: &AppHandle) {
    if mark_unconfirmed(pending, app_handle) {
        crate::show_main_window(app_handle);
    }
}

fn spawn_waiter(pending: Arc<Mutex<MacPending>>, app_handle: AppHandle) {
    thread::spawn(move || {
        let outcome = loop {
            thread::sleep(Duration::from_millis(15));
            let decision = {
                let p = match pending.lock() {
                    Ok(p) => p,
                    Err(_) => return,
                };
                if p.settled {
                    // A newer paste settled this transaction already.
                    return;
                }
                let result = match p.state.lock() {
                    Ok(st) => evaluate_mac(&st, Instant::now()),
                    Err(_) => return,
                };
                result
            };
            if decision != MacWaitDecision::KeepWaiting {
                break decision;
            }
        };
        info!("[reliable-paste] transaction outcome: {outcome:?}");

        let pending_for_finish = pending.clone();
        let app_for_finish = app_handle.clone();
        if let Err(error) = app_handle.run_on_main_thread(move || {
            if outcome == MacWaitDecision::Unconfirmed {
                let preserve = pending_for_finish
                    .lock()
                    .map(|p| p.preserve_transcript)
                    .unwrap_or(false);
                if preserve {
                    // An intentionally retained transcript must become normal
                    // clipboard content even when injection was unconfirmed.
                    if settle(&pending_for_finish, &app_for_finish, false).is_ok() {
                        clear_completed(&pending_for_finish);
                        let _ = app_for_finish.emit("paste-error", ());
                        return;
                    }
                }
                report_unconfirmed(&pending_for_finish, &app_for_finish);
            } else {
                match settle(&pending_for_finish, &app_for_finish, false) {
                    Ok(()) => clear_completed(&pending_for_finish),
                    Err(error) => {
                        error!("[reliable-paste] settlement failed: {error}");
                        report_unconfirmed(&pending_for_finish, &app_for_finish);
                    }
                }
            }
        }) {
            error!("[reliable-paste] could not schedule settlement: {error}");
            mark_unconfirmed(&pending, &app_handle);
        }
    });
}

pub(super) fn run(
    text: &str,
    app_handle: &AppHandle,
    paste_method: &PasteMethod,
    _enigo: &mut enigo::Enigo,
    auto_submit: bool,
    auto_submit_key: AutoSubmitKey,
    clipboard_handling: ClipboardHandling,
) -> Result<(), String> {
    // Refuse overlap rather than prematurely restoring or replacing data a
    // queued paste may still read. A new external copy releases the slot.
    let mut slot = PENDING
        .lock()
        .map_err(|_| "Paste transaction lock failed")?;
    if let Some(previous) = slot.as_ref() {
        let p = previous
            .lock()
            .map_err(|_| "Paste transaction lock failed")?;
        if !p.settled && NSPasteboard::generalPasteboard().changeCount() == p.change_count {
            return Err("Another paste is still pending; recover it before pasting again".into());
        }
        let _ = app_handle.emit("paste-resolved", p.id);
    }
    *slot = None;

    let target_pid = foreground_pid().ok_or("Cannot identify the active paste target")?;
    let pasteboard = NSPasteboard::generalPasteboard();
    let snapshot_count = pasteboard.changeCount();
    let snapshot = ClipboardSnapshot::capture(&pasteboard)?;
    if pasteboard.changeCount() != snapshot_count {
        return Err("Clipboard changed while preparing the paste".into());
    }
    if foreground_pid() != Some(target_pid) {
        return Err("Active application changed while preparing the paste".into());
    }

    let state = Arc::new(Mutex::new(TxState::new()));
    let receipt_state = state.clone();
    let ownership_state = state.clone();
    let provider = HandyPasteProvider::new(
        text.to_string(),
        Box::new(move || {
            if let Ok(mut state) = receipt_state.lock() {
                state.record_receipt(Instant::now());
            }
        }),
        Box::new(move || {
            if let Ok(mut state) = ownership_state.lock() {
                state.ownership_lost = true;
            }
        }),
    );
    let change_count = publish(&pasteboard, &provider)?;
    info!("[reliable-paste] published transcript as lazy promise (changeCount {change_count})");

    let pending = Arc::new(Mutex::new(MacPending {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        state,
        snapshot,
        target_pid,
        change_count,
        provider: Some(provider),
        auto_submit,
        auto_submit_key,
        preserve_transcript: clipboard_handling == ClipboardHandling::CopyToClipboard,
        transcript: text.to_string(),
        settled: false,
        unconfirmed: false,
    }));
    *slot = Some(pending.clone());
    drop(slot);

    // Respect the configured pre-paste delay without blocking AppKit's run
    // loop. A clipboard observer can request data during this interval; that
    // pre-injection receipt must not count as target delivery.
    let delay = get_settings(app_handle).paste_delay_ms;
    let method = *paste_method;
    let app = app_handle.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(delay));
        let app_for_injection = app.clone();
        let pending_for_injection = pending.clone();
        if let Err(error) = app.run_on_main_thread(move || {
            let p = match pending_for_injection.lock() {
                Ok(p) => p,
                Err(_) => return,
            };
            let board = NSPasteboard::generalPasteboard();
            let target_unchanged = foreground_pid() == Some(p.target_pid);
            if let Ok(mut state) = p.state.lock() {
                state.ownership_lost |= board.changeCount() != p.change_count;
                if !target_unchanged {
                    state.cancelled = true;
                }
            }
            let can_inject = p
                .state
                .lock()
                .map(|st| !st.ownership_lost && !st.cancelled)
                .unwrap_or(false);
            if !p.settled && can_inject {
                let injected = Instant::now();
                if let Ok(mut state) = p.state.lock() {
                    state.injected_at = Some(injected);
                    state.published_at = injected; // full delivery window after injection
                }
                let result = app_for_injection
                    .try_state::<EnigoState>()
                    .ok_or_else(|| "Input manager unavailable".to_string())
                    .and_then(|input| {
                        input
                            .0
                            .lock()
                            .map_err(|_| "Input manager lock failed".to_string())
                            .and_then(|mut enigo| send_chord(&mut enigo, &method))
                    });
                if let Err(error) = result {
                    if let Ok(mut state) = p.state.lock() {
                        state.injection_failed = true;
                    }
                    error!("[reliable-paste] chord injection failed: {error}");
                } else {
                    info!("[reliable-paste] paste chord sent ({method:?})");
                }
            }
            // Explicit recovery may have settled the transaction during the delay.
            let settled = p.settled;
            drop(p);
            if !settled {
                spawn_waiter(pending_for_injection, app_for_injection);
            }
        }) {
            error!("[reliable-paste] could not schedule injection: {error}");
            mark_unconfirmed(&pending, &app);
        }
    });

    Ok(())
}

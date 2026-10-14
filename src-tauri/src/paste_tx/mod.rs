//! Receipt-sequenced clipboard paste ("reliable paste", debug-gated).
//!
//! The legacy clipboard paste (`clipboard::paste_via_clipboard`) restores the
//! previous clipboard after a fixed delay. The paste keystroke is only
//! *enqueued* at that point — the target application reads the clipboard
//! whenever its event loop gets to it, so any fixed delay can lose the race
//! and the user gets their old clipboard pasted back (#502).
//!
//! This module instead publishes the transcript as a *lazy promise* and waits
//! for the operating system to tell us that a consumer actually read the
//! clipboard — a "receipt" — before restoring:
//!
//! - Windows: delayed rendering (`SetClipboardData(CF_UNICODETEXT, NULL)`),
//!   the owner window receives `WM_RENDERFORMAT` on read.
//! - macOS: `declareTypes:owner:` with an owner object, the pasteboard calls
//!   `pasteboard:provideDataForType:` on read.
//!
//! Receipts indicate data provision, not successful insertion or reader identity.
//! macOS clipboard managers honoring the transient markers do not request text.
//! An unrelated text reader can still consume a promise, and AppKit may cache
//! that data without calling the provider again for the eventual paste target.
//! Two guards reduce premature restoration:
//!
//! 1. Only receipts observed *after* the paste chord was injected count. A
//!    read before that is an eager third party (clipboard manager, antivirus)
//!    reacting to the clipboard change itself.
//! 2. Restoration only happens while we still own the clipboard
//!    (sequence number / changeCount unchanged, no ownership-lost event). If
//!    the user copied something else in the meantime, their action wins.
//!
//! The restore is additionally gated on a short quiet period after the *last*
//! receipt, because some applications read the clipboard several times per
//! paste. The macOS timeout reports an unconfirmed paste and retains the
//! temporary item for explicit recovery; it must not restore on a timer.
//! Windows retains its existing timeout policy.

// The shared transaction state is compiled on all platforms (for the unit
// tests), but only the macOS/Windows platform modules consume all of it.
#![cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]

use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod macos_clipboard;
#[cfg(target_os = "macos")]
pub(crate) use macos::{cancel_pending, pending_id, restore_pending};
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
use windows as platform;

/// How long after the *last* observed read the transcript stays on the
/// clipboard before restoring. Covers applications that read the clipboard
/// several times per paste (e.g. Chromium probe-then-read).
pub(crate) const QUIET_PERIOD: Duration = Duration::from_millis(200);

/// Upper bound on how long the transcript may occupy the clipboard before we
/// restore regardless of receipts. Long enough that a realistically loaded
/// target always gets to read first; short enough that a lost keystroke does
/// not strand the transcript on the clipboard for long.
pub(crate) const RESTORE_TIMEOUT: Duration = Duration::from_secs(8);

/// When the chord could not be injected at all, no legitimate receipt can
/// arrive, so restore quickly instead of waiting out the full timeout.
#[cfg(any(target_os = "windows", test))]
pub(crate) const FAILED_INJECTION_TIMEOUT: Duration = Duration::from_millis(500);

/// Shared, cross-thread record of one paste transaction.
#[derive(Debug)]
pub(crate) struct TxState {
    /// When the transcript was published to the clipboard.
    pub published_at: Instant,
    /// When the paste chord was injected. Only receipts *after* this count as
    /// evidence the target read the transcript — earlier reads are eager third
    /// parties reacting to the clipboard change itself.
    pub injected_at: Option<Instant>,
    /// The chord could not be sent; short-circuit the wait.
    pub injection_failed: bool,
    /// Times at which a consumer requested the clipboard data.
    pub receipts: Vec<Instant>,
    /// Someone else took clipboard ownership (user copied elsewhere, ...).
    pub ownership_lost: bool,
    /// A newer paste transaction settled this one early (see flush logic in
    /// the platform modules).
    pub cancelled: bool,
    /// The post-paste Enter (auto-submit) has been sent for this transaction.
    /// (Read on Windows; the macOS path settles via `MacPending::settled`.)
    #[allow(dead_code)]
    pub auto_submit_sent: bool,
    /// First post-injection receipt has been logged.
    pub logged_receipt: bool,
}

impl TxState {
    pub fn new() -> Self {
        Self {
            published_at: Instant::now(),
            injected_at: None,
            injection_failed: false,
            receipts: Vec::new(),
            ownership_lost: false,
            cancelled: false,
            auto_submit_sent: false,
            logged_receipt: false,
        }
    }

    /// Records a read receipt, logging the first one that counts as evidence.
    pub fn record_receipt(&mut self, at: Instant) {
        self.receipts.push(at);
        if !self.logged_receipt {
            if let Some(injected) = self.injected_at {
                if at >= injected {
                    self.logged_receipt = true;
                    log::info!(
                        "[reliable-paste] clipboard read {}ms after chord",
                        at.duration_since(injected).as_millis()
                    );
                }
            }
        }
    }

    pub fn last_receipt_after_injection(&self) -> Option<Instant> {
        let injected = self.injected_at?;
        self.receipts.iter().copied().rev().find(|t| *t >= injected)
    }

    pub fn any_receipt_after_injection(&self) -> bool {
        self.last_receipt_after_injection().is_some()
    }
}

#[cfg(any(target_os = "windows", test))]
pub(crate) enum WaitDecision {
    KeepWaiting,
    /// Stop waiting; settle the transaction (auto-submit + guarded restore).
    Finish,
}

/// macOS distinguishes successful data provision from an expired wait. A
/// deadline is not permission to restore the clipboard under a queued paste.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq)]
pub(crate) enum MacWaitDecision {
    KeepWaiting,
    Delivered,
    OwnershipLost,
    Unconfirmed,
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn evaluate_mac(state: &TxState, now: Instant) -> MacWaitDecision {
    if state.ownership_lost {
        return MacWaitDecision::OwnershipLost;
    }
    // Key injection may have partially succeeded. Never restore on that error.
    if state.injection_failed || state.cancelled {
        return MacWaitDecision::Unconfirmed;
    }
    if let Some(last) = state.last_receipt_after_injection() {
        if now.duration_since(last) >= QUIET_PERIOD {
            return MacWaitDecision::Delivered;
        }
    }
    if now.duration_since(state.published_at) >= RESTORE_TIMEOUT {
        return MacWaitDecision::Unconfirmed;
    }
    MacWaitDecision::KeepWaiting
}

/// Pure decision: given the current transaction state, keep waiting for the
/// target to read, or finish now. Both platform event loops call this.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn evaluate(state: &TxState, now: Instant) -> WaitDecision {
    if state.ownership_lost || state.cancelled {
        return WaitDecision::Finish;
    }
    if let Some(last) = state.last_receipt_after_injection() {
        if now.duration_since(last) >= QUIET_PERIOD {
            return WaitDecision::Finish;
        }
    }
    let deadline = if state.injection_failed {
        FAILED_INJECTION_TIMEOUT
    } else {
        RESTORE_TIMEOUT
    };
    if now.duration_since(state.published_at) >= deadline {
        return WaitDecision::Finish;
    }
    WaitDecision::KeepWaiting
}

/// Modifier hold for the receipt-sequenced chord, kept at parity with the
/// legacy path (100ms) for the beta: that hold was added in #165 because real
/// users' systems dropped chords released too quickly, and the beta should
/// validate the receipt mechanism without changing a second variable.
///
/// Once receipts are proven in the field this becomes a safe tuning knob — a
/// chord the target never recognizes produces no receipt and is logged ("no
/// read within timeout") rather than failing silently, so a shorter hold
/// (measured working at 10ms on a fast machine, cutting visible latency from
/// ~110ms to ~20ms) can be tried as its own experiment later.
const CHORD_HOLD_MS: u64 = 100;

/// Sends the platform paste chord for the configured method.
pub(crate) fn send_chord(
    enigo: &mut enigo::Enigo,
    paste_method: &crate::settings::PasteMethod,
) -> Result<(), String> {
    use crate::settings::PasteMethod;
    match paste_method {
        PasteMethod::CtrlV => crate::input::send_paste_ctrl_v(enigo, CHORD_HOLD_MS),
        PasteMethod::CtrlShiftV => crate::input::send_paste_ctrl_shift_v(enigo, CHORD_HOLD_MS),
        PasteMethod::ShiftInsert => crate::input::send_paste_shift_insert(enigo, CHORD_HOLD_MS),
        other => Err(format!(
            "Invalid paste method for clipboard paste: {:?}",
            other
        )),
    }
}

/// Attempts the receipt-sequenced paste. Returns `Err` before anything has
/// been published when the platform transaction cannot start. macOS callers
/// must surface that error without falling back to timed restoration. On
/// `Ok`, the transaction has started; data delivery is reported asynchronously.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn try_reliable_paste(
    text: &str,
    app_handle: &tauri::AppHandle,
    paste_method: &crate::settings::PasteMethod,
    enigo: &mut enigo::Enigo,
    auto_submit: bool,
    auto_submit_key: crate::settings::AutoSubmitKey,
    clipboard_handling: crate::settings::ClipboardHandling,
) -> Result<(), String> {
    platform::run(
        text,
        app_handle,
        paste_method,
        enigo,
        auto_submit,
        auto_submit_key,
        clipboard_handling,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_after_publish(published_ago: Duration) -> TxState {
        let mut s = TxState::new();
        s.published_at = Instant::now() - published_ago;
        s
    }

    #[test]
    fn keeps_waiting_without_receipt_within_timeout() {
        let s = state_after_publish(Duration::from_millis(100));
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));
    }

    #[test]
    fn finishes_after_quiet_period_once_read() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.injected_at = Some(Instant::now() - Duration::from_millis(250));
        s.receipts.push(Instant::now() - QUIET_PERIOD);
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn waits_through_quiet_period_after_recent_read() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.injected_at = Some(Instant::now() - Duration::from_millis(100));
        s.receipts.push(Instant::now() - Duration::from_millis(50));
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));
    }

    #[test]
    fn pre_injection_receipt_does_not_count() {
        let mut s = state_after_publish(Duration::from_millis(300));
        s.receipts.push(Instant::now() - Duration::from_millis(200));
        s.injected_at = Some(Instant::now() - Duration::from_millis(100));
        assert!(!s.any_receipt_after_injection());
        assert!(matches!(
            evaluate(&s, Instant::now()),
            WaitDecision::KeepWaiting
        ));
    }

    #[test]
    fn finishes_on_timeout_without_receipt() {
        let s = state_after_publish(RESTORE_TIMEOUT);
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn failed_injection_uses_short_timeout() {
        let mut s = state_after_publish(FAILED_INJECTION_TIMEOUT);
        s.injection_failed = true;
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn ownership_loss_finishes_immediately() {
        let mut s = state_after_publish(Duration::from_millis(10));
        s.ownership_lost = true;
        assert!(matches!(evaluate(&s, Instant::now()), WaitDecision::Finish));
    }

    #[test]
    fn mac_timeout_retains_unconfirmed_clipboard() {
        let s = state_after_publish(RESTORE_TIMEOUT);
        assert_eq!(
            evaluate_mac(&s, Instant::now()),
            MacWaitDecision::Unconfirmed
        );
    }

    #[test]
    fn mac_cancellation_does_not_restore_under_an_enqueued_paste() {
        let mut s = state_after_publish(Duration::from_secs(1));
        s.cancelled = true;
        assert_eq!(
            evaluate_mac(&s, Instant::now()),
            MacWaitDecision::Unconfirmed
        );
    }

    #[test]
    fn mac_partial_injection_does_not_restore_even_with_a_read() {
        let mut s = state_after_publish(Duration::from_secs(1));
        s.injected_at = Some(s.published_at);
        s.receipts.push(s.published_at + Duration::from_millis(10));
        s.injection_failed = true;
        assert_eq!(
            evaluate_mac(&s, Instant::now()),
            MacWaitDecision::Unconfirmed
        );
    }

    #[test]
    fn mac_eager_reader_does_not_authorize_restoration() {
        let mut s = state_after_publish(RESTORE_TIMEOUT);
        s.receipts.push(s.published_at);
        s.injected_at = Some(s.published_at + Duration::from_millis(10));
        assert_eq!(
            evaluate_mac(&s, Instant::now()),
            MacWaitDecision::Unconfirmed
        );
    }

    #[test]
    fn mac_delivery_waits_for_quiet_and_new_copy_wins() {
        let mut s = state_after_publish(Duration::from_secs(1));
        s.injected_at = Some(s.published_at);
        s.receipts.push(Instant::now());
        assert_eq!(
            evaluate_mac(&s, Instant::now()),
            MacWaitDecision::KeepWaiting
        );
        s.receipts[0] = Instant::now() - QUIET_PERIOD;
        assert_eq!(evaluate_mac(&s, Instant::now()), MacWaitDecision::Delivered);
        s.ownership_lost = true;
        assert_eq!(
            evaluate_mac(&s, Instant::now()),
            MacWaitDecision::OwnershipLost
        );
    }
}

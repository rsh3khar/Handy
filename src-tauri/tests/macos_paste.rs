//! AppKit promises need a main run loop. Rust's ordinary test threads cannot
//! service their callbacks. This executable tests production native helpers
//! on the main thread using named pasteboards and synthetic data exclusively.
#[cfg(target_os = "macos")]
#[path = "../src/paste_tx/macos_clipboard.rs"]
mod native;

#[cfg(target_os = "macos")]
fn main() {
    use native::{publish, ClipboardSnapshot, HandyPasteProvider, CONCEALMENT_TYPES};
    use objc2::msg_send;
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::{NSDate, NSRunLoop, NSString};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("read") {
        std::thread::sleep(Duration::from_millis(args[3].parse().expect("delay")));
        let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&args[2]));
        let value = board.stringForType(&NSString::from_str("public.utf8-plain-text"));
        assert_eq!(
            value.map(|s| s.to_string()),
            Some("synthetic transcript".into())
        );
        return;
    }

    for delay in [50, 100, 300, 1000] {
        let name = format!("HandyNativePaste-{}-{delay}", std::process::id());
        let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&name));
        board.clearContents();
        board.setString_forType(
            &NSString::from_str("synthetic old"),
            &NSString::from_str("public.utf8-plain-text"),
        );
        let snapshot = ClipboardSnapshot::capture(&board).expect("capture");
        let receipts = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let receipt_sink = receipts.clone();
        let provider = HandyPasteProvider::new(
            "synthetic transcript".into(),
            Box::new(move || {
                receipt_sink
                    .lock()
                    .expect("receipt lock")
                    .push(Instant::now())
            }),
            Box::new(|| {}),
        );
        let count = publish(&board, &provider).expect("publish");
        // Maccy checks types first. Reading the marker must not be delivery.
        assert!(board
            .types()
            .expect("types")
            .iter()
            .any(|t| t.to_string() == CONCEALMENT_TYPES[0]));
        board.dataForType(&NSString::from_str(CONCEALMENT_TYPES[0]));
        assert!(receipts.lock().expect("receipts").is_empty());
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args(["read", &name, &delay.to_string()])
            .stdout(Stdio::null())
            .spawn()
            .expect("start slow consumer");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut restored = false;
        while Instant::now() < deadline {
            NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.005));
            let ready = receipts
                .lock()
                .expect("receipts")
                .last()
                .is_some_and(|at| at.elapsed() >= Duration::from_millis(200));
            if ready && !restored {
                assert_eq!(board.changeCount(), count);
                assert!(snapshot
                    .restore_if_owned(&board, count)
                    .expect("restore after native provision"));
                restored = true;
            }
            if restored && child.try_wait().expect("status").is_some() {
                break;
            }
        }
        if child.try_wait().expect("status").is_none() {
            child.kill().expect("stop test consumer");
        }
        assert!(child.wait().expect("consumer result").success());
        assert!(restored, "native promise never fulfilled");
        assert_eq!(
            board
                .stringForType(&NSString::from_str("public.utf8-plain-text"))
                .map(|s| s.to_string()),
            Some("synthetic old".into())
        );
        println!("PASS production provider: delayed consumer {delay}ms; marker did not count as receipt; prior clipboard restored");
        unsafe {
            let _: () = msg_send![&*board, releaseGlobally];
        }
    }

    let observer_name = format!("HandyObserver-{}", std::process::id());
    let observer_board = NSPasteboard::pasteboardWithName(&NSString::from_str(&observer_name));
    observer_board.clearContents();
    observer_board.setString_forType(
        &NSString::from_str("synthetic old"),
        &NSString::from_str("public.utf8-plain-text"),
    );
    let saved = ClipboardSnapshot::capture(&observer_board).expect("capture before observer");
    let reads = Arc::new(Mutex::new(0));
    let sink = reads.clone();
    let lost = Arc::new(Mutex::new(false));
    let lost_sink = lost.clone();
    let owner = HandyPasteProvider::new(
        "synthetic transcript".into(),
        Box::new(move || *sink.lock().expect("read counter") += 1),
        Box::new(move || *lost_sink.lock().expect("ownership counter") = true),
    );
    let count = publish(&observer_board, &owner).expect("publish observer case");
    assert_eq!(
        observer_board
            .stringForType(&NSString::from_str("public.utf8-plain-text"))
            .map(|v| v.to_string()),
        Some("synthetic transcript".into())
    );
    assert_eq!(*reads.lock().expect("counter"), 1);
    let mut second = Command::new(std::env::current_exe().expect("test executable"))
        .args(["read", &observer_name, "100"])
        .stdout(Stdio::null())
        .spawn()
        .expect("start cached consumer");
    let deadline = Instant::now() + Duration::from_secs(3);
    while second.try_wait().expect("consumer status").is_none() && Instant::now() < deadline {
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.005));
    }
    if second.try_wait().expect("status").is_none() {
        second.kill().expect("stop test consumer");
    }
    assert!(second.wait().expect("result").success());
    assert_eq!(
        *reads.lock().expect("counter"),
        1,
        "AppKit caches the text: a later target need not produce another callback"
    );
    observer_board.clearContents();
    observer_board.setString_forType(
        &NSString::from_str("synthetic new user copy"),
        &NSString::from_str("public.utf8-plain-text"),
    );
    assert!(!saved
        .restore_if_owned(&observer_board, count)
        .expect("guarded restore"));
    assert_eq!(
        observer_board
            .stringForType(&NSString::from_str("public.utf8-plain-text"))
            .map(|v| v.to_string()),
        Some("synthetic new user copy".into())
    );
    println!("PASS eager observer: cached reads produce one callback; new external copy survives guarded restoration");
    unsafe {
        let _: () = msg_send![&*observer_board, releaseGlobally];
    }

    let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&format!(
        "HandySnapshot-{}",
        std::process::id()
    )));
    let snapshot = ClipboardSnapshot(vec![
        vec![
            ("public.utf8-plain-text".into(), b"synthetic text".to_vec()),
            ("public.rtf".into(), br"{\rtf1 synthetic text}".to_vec()),
        ],
        vec![("org.handy.synthetic.bytes".into(), vec![0, 1, 255, 2])],
    ]);
    snapshot
        .restore(&board)
        .expect("write original synthetic data");
    let captured = ClipboardSnapshot::capture(&board).expect("capture all formats");
    board.clearContents();
    board.setString_forType(
        &NSString::from_str("temporary"),
        &NSString::from_str("public.utf8-plain-text"),
    );
    captured.restore(&board).expect("restore all formats");
    assert_eq!(
        ClipboardSnapshot::capture(&board).expect("read formats"),
        captured
    );
    println!("PASS production snapshot: multiple items, plain text, rich text, binary bytes");
    unsafe {
        let _: () = msg_send![&*board, releaseGlobally];
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}

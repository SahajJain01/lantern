use super::*;

fn online(serial: &str) -> DeviceEvent {
    DeviceEvent::Online(serial.into())
}

fn offline(serial: &str) -> DeviceEvent {
    DeviceEvent::Offline(serial.into())
}

/// Feeds each chunk as one read() result, exactly as the pipe would deliver it.
fn events(chunks: &[&[u8]]) -> Vec<DeviceEvent> {
    let mut decoder = TrackDevicesDecoder::default();
    let mut tracker = DeviceTracker::default();
    let mut out = Vec::new();
    for chunk in chunks {
        for list in decoder.push(chunk).unwrap() {
            out.extend(tracker.update(&list));
        }
    }
    out
}

fn msg(payload: &str) -> Vec<u8> {
    format!("{:04x}{payload}", payload.len()).into_bytes()
}

// Captured with `adb track-devices | xxd` (adb 1.0.41 / 37.0.1) while 20e9318 was attached.
const CAPTURED_ONLINE: &[u8] = b"000f20e9318\tdevice\n";
// Captured across `adb -s 20e9318 reconnect`: device, offline, then the empty list (`0000`, no
// newline). This is the exact byte stream; the removal is only visible as the empty list.
const CAPTURED_RECONNECT: &[u8] = b"000f20e9318\tdevice\n001020e9318\toffline\n0000";
// Captured by starting `adb track-devices` while the phone was absent, then `adb kill-server`:
// the whole stream was these four bytes followed by EOF.
const CAPTURED_ABSENT_THEN_KILL: &[u8] = b"0000";

#[test]
fn parse_track_devices_lines() {
    assert_eq!(events(&[CAPTURED_ONLINE]), [online("20e9318")]);
}

#[test]
fn captured_reconnect_goes_offline_once() {
    assert_eq!(
        events(&[CAPTURED_RECONNECT]),
        [online("20e9318"), offline("20e9318")]
    );
}

#[test]
fn empty_list_after_device_is_unplug() {
    assert_eq!(
        events(&[CAPTURED_ONLINE, CAPTURED_ABSENT_THEN_KILL]),
        [online("20e9318"), offline("20e9318")]
    );
    let mut decoder = TrackDevicesDecoder::default();
    assert_eq!(
        decoder.push(b"0000").unwrap(),
        vec![Vec::<DeviceEntry>::new()]
    );
}

#[test]
fn empty_list_glued_to_next_message_is_replug() {
    assert_eq!(
        events(&[CAPTURED_ONLINE, b"0000000f20e9318\tdevice\n"]),
        [online("20e9318"), offline("20e9318"), online("20e9318")]
    );
}

#[test]
fn two_entries_with_hex_looking_serials_keep_full_serials() {
    let both = msg("abcd1234\tdevice\n0123456789ABCDEF\tdevice\n");
    let one = msg("abcd1234\tdevice\n");
    assert_eq!(&both[..4], b"0028");
    assert_eq!(
        events(&[&both, &one]),
        [
            online("0123456789ABCDEF"),
            online("abcd1234"),
            offline("0123456789ABCDEF"),
        ]
    );
}

#[test]
fn message_split_across_reads_is_reassembled() {
    let mut stream = Vec::new();
    stream.extend_from_slice(CAPTURED_RECONNECT);
    stream.extend_from_slice(CAPTURED_ONLINE);
    let chunks: Vec<&[u8]> = stream.chunks(1).collect();
    assert_eq!(
        events(&chunks),
        [online("20e9318"), offline("20e9318"), online("20e9318")]
    );
    assert_eq!(
        events(&[b"00", b"0f20e93", b"18\tdev", b"ice\n"]),
        [online("20e9318")]
    );
}

#[test]
fn non_device_states_are_not_online() {
    for state in [
        "authorizing",
        "connecting",
        "recovery",
        "unauthorized",
        "offline",
        "sideload",
        "bootloader",
    ] {
        let m = msg(&format!("20e9318\t{state}\n"));
        assert_eq!(events(&[&m]), [], "state {state}");
        assert_eq!(
            events(&[CAPTURED_ONLINE, &m, CAPTURED_ONLINE]),
            [online("20e9318"), offline("20e9318"), online("20e9318")],
            "state {state}"
        );
    }
    let no_perm = msg("20e9318\tno permissions (missing udev rules? user is in the plugdev group); see [http://developer.android.com/tools/device.html]\n");
    assert_eq!(events(&[&no_perm]), []);
    assert_eq!(
        events(&[CAPTURED_ONLINE, &no_perm]),
        [online("20e9318"), offline("20e9318")]
    );
    let mut decoder = TrackDevicesDecoder::default();
    let lists = decoder.push(&no_perm).unwrap();
    assert_eq!(lists[0][0].serial, "20e9318");
    assert!(lists[0][0]
        .state
        .starts_with("no permissions (missing udev rules?"));
}

#[test]
fn repeated_lists_do_not_repeat_events() {
    assert_eq!(
        events(&[CAPTURED_ONLINE, CAPTURED_ONLINE, b"0000", b"0000"]),
        [online("20e9318"), offline("20e9318")]
    );
}

#[test]
fn stream_end_marks_everything_offline() {
    let mut decoder = TrackDevicesDecoder::default();
    let mut tracker = DeviceTracker::default();
    for list in decoder.push(CAPTURED_ONLINE).unwrap() {
        tracker.update(&list);
    }
    assert_eq!(tracker.update(&[]), [offline("20e9318")]);
}

// Same line format as `adb forward --list` prints (checked against adb 1.0.41 with 20e9318
// attached: `20e9318 tcp:44795 localabstract:lantern-83196eb5`). The list covers every device.
const FORWARD_LIST: &str = "20e9318 tcp:46791 localabstract:lantern-7b54a3f9\n\
other99 tcp:5000 localabstract:lantern-deadbeef\n\
20e9318 tcp:8080 tcp:8080\n\
20e9318 tcp:6000 localabstract:scrcpy_1234\n\
\n\
20e9318 tcp:39207 localabstract:lantern-00c0ffee\n";

#[test]
fn stale_lantern_forwards_are_this_serials_lantern_sockets_only() {
    assert_eq!(
        stale_lantern_forwards(FORWARD_LIST, "20e9318"),
        ["tcp:46791", "tcp:39207"]
    );
    assert_eq!(
        stale_lantern_forwards(FORWARD_LIST, "other99"),
        ["tcp:5000"]
    );
    assert!(stale_lantern_forwards(FORWARD_LIST, "20e931").is_empty());
    assert!(stale_lantern_forwards("", "20e9318").is_empty());
    assert!(stale_lantern_forwards("garbage\n\n", "20e9318").is_empty());
}

#[test]
fn non_hex_length_is_an_error() {
    let mut decoder = TrackDevicesDecoder::default();
    assert!(decoder.push(b"* daemon not running\n").is_err());
}

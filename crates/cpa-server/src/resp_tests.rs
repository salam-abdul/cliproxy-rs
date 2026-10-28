use super::*;
use std::time::Duration;

#[tokio::test]
async fn rejects_excessive_counts_and_lengths_before_reading_payload() {
    let cases: &[(&[u8], &str)] = &[
        (b"*1025\r\n", "protocol error"),
        (b"*9223372036854775807\r\n", "protocol error"),
        (b"*1\r\n$65537\r\n", "bulk string too large"),
        (b"*1\r\n$9223372036854775806\r\n", "bulk string too large"),
        (b"*1\r\n$-2\r\n", "protocol error"),
    ];
    for (frame, expected) in cases {
        let (mut peer, stream) = tokio::io::duplex(frame.len() + 1);
        peer.write_all(frame).await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), read_command(&mut BufReader::new(stream)))
            .await
            .expect("reject the declaration without waiting for a payload or EOF")
            .unwrap_err();
        assert_eq!(error.message().as_deref(), Some(*expected));
    }
}

#[tokio::test]
async fn rejects_unterminated_lines_without_waiting_for_eof() {
    let mut frame = b"*1\r\n+".to_vec();
    frame.extend(vec![b'x'; 65539]);
    let (mut peer, stream) = tokio::io::duplex(frame.len() + 1);
    peer.write_all(&frame).await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(1), read_command(&mut BufReader::new(stream)))
        .await
        .expect("bound an unterminated line while the peer is still connected")
        .unwrap_err();
    assert_eq!(error.message().as_deref(), Some("protocol error"));

    let mut count = vec![b'*'];
    count.extend(vec![b'0'; 65537]);
    count.push(b'\n');
    let error = read_command(&mut BufReader::new(count.as_slice())).await.unwrap_err();
    assert_eq!(error.message().as_deref(), Some("protocol error"));
}

#[tokio::test]
async fn enforces_cumulative_payload_budget_for_bulk_and_inline_strings() {
    let mut bulk = b"*2\r\n$65536\r\n".to_vec();
    bulk.extend(vec![b'x'; 65536]);
    bulk.extend_from_slice(b"\r\n$1\r\n");
    let (mut peer, stream) = tokio::io::duplex(bulk.len() + 1);
    peer.write_all(&bulk).await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(1), read_command(&mut BufReader::new(stream)))
        .await
        .expect("reject the second bulk length before reading its payload")
        .unwrap_err();
    assert_eq!(error.message().as_deref(), Some("bulk string too large"));

    for suffix in [b"\r\n+1\r\n", b"\r\n:1\r\n"] {
        let mut frame = b"*2\r\n+".to_vec();
        frame.extend(vec![b'x'; 65536]);
        frame.extend_from_slice(suffix);
        let error = read_command(&mut BufReader::new(frame.as_slice())).await.unwrap_err();
        assert_eq!(error.message().as_deref(), Some("protocol error"));
    }
}

#[tokio::test]
async fn accepts_exact_limits_and_resets_budget_for_each_pipelined_command() {
    let mut frame = b"*1\r\n$65536\r\n".to_vec();
    frame.extend(vec![b'x'; 65536]);
    frame.extend_from_slice(b"\r\n*1\r\n+");
    frame.extend(vec![b'z'; 65536]);
    frame.extend_from_slice(b"\r\n*1024\r\n");
    for _ in 0..1024 {
        frame.extend_from_slice(b"+\r\n");
    }
    frame.extend_from_slice(b"*2\r\n+");
    frame.extend(vec![0xff; 32768]);
    frame.extend_from_slice(b"\r\n$32768\r\n");
    frame.extend(vec![b'y'; 32768]);
    frame.extend_from_slice(b"\r\n*1\r\n+PING\r\n");
    let mut reader = BufReader::with_capacity(7, frame.as_slice());
    assert_eq!(read_command(&mut reader).await.unwrap(), ["x".repeat(65536)]);
    assert_eq!(read_command(&mut reader).await.unwrap(), ["z".repeat(65536)]);
    assert_eq!(read_command(&mut reader).await.unwrap(), vec![String::new(); 1024]);
    assert_eq!(
        read_command(&mut reader).await.unwrap(),
        ["�".repeat(32768), "y".repeat(32768)]
    );
    assert_eq!(read_command(&mut reader).await.unwrap(), ["PING"]);
}

#[tokio::test]
async fn preserves_null_empty_and_truncated_frame_behavior() {
    let mut reader = BufReader::new(&b"*3\r\n$-1\r\n$0\r\n\r\n:1\r\n*0\r\n"[..]);
    assert_eq!(read_command(&mut reader).await.unwrap(), ["", "", "1"]);
    assert!(read_command(&mut reader).await.unwrap().is_empty());
    for (frame, message) in [
        (&b"*1\r\n$3\r\n"[..], None),
        (&b"*1\r\n$3\r\nx"[..], Some("unexpected EOF")),
        (&b"*1\r\n$0\r\nxx"[..], Some("protocol error")),
        (&b"*1\r\n+cut"[..], None),
    ] {
        assert_eq!(
            read_command(&mut BufReader::new(frame))
                .await
                .unwrap_err()
                .message()
                .as_deref(),
            message
        );
    }
}

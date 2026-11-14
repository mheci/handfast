//! Independent payload sender and receiver behavior.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use handfast_testkit::frame::TestkitError;
use handfast_testkit::payload_peer::{
    run_payload_self_test, PayloadReceiveReport, PayloadReceiver, PayloadReceiverConfig,
    PayloadSendReport, PayloadSender, PayloadSenderConfig,
};
use handfast_testkit::tls_peer::{CertificateKind, TlsVersion};

type SendResult = Result<PayloadSendReport, TestkitError>;
type ReceiveResult = Result<PayloadReceiveReport, TestkitError>;

fn address(sender: &PayloadSender) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], sender.port()))
}

async fn transfer(
    sender_config: PayloadSenderConfig,
    receiver_config: PayloadReceiverConfig,
    payload: Vec<u8>,
    max_size: u64,
) -> (SendResult, ReceiveResult, Vec<u8>) {
    let sender = PayloadSender::new(sender_config).unwrap();
    let receiver = PayloadReceiver::new(receiver_config).unwrap();
    let mut buffer = Vec::new();
    let (send, receive) = tokio::join!(
        sender.accept_and_send(&payload),
        receiver.receive(address(&sender), max_size, &mut buffer),
    );
    (send, receive, buffer)
}

#[tokio::test]
async fn happy_path_moves_zero_one_one_mib_and_eight_mib() {
    for size in [0usize, 1, 1024 * 1024, 8 * 1024 * 1024] {
        let payload: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
        let (send, receive, buffer) = transfer(
            PayloadSenderConfig::default(),
            PayloadReceiverConfig::default(),
            payload.clone(),
            size as u64,
        )
        .await;
        let send = send.unwrap();
        let receive = receive.unwrap();
        assert_eq!(send.bytes, size as u64, "{size} bytes sent");
        assert_eq!(receive.bytes, size as u64, "{size} bytes received");
        assert!(receive.eof, "{size} bytes reached EOF");
        assert_eq!(buffer, payload, "{size} bytes match");
    }
}

#[tokio::test]
async fn tls_12_and_13_work_with_ecdsa_and_rsa() {
    for version in [TlsVersion::Tls12, TlsVersion::Tls13] {
        for certificate in [CertificateKind::Ecdsa, CertificateKind::Rsa] {
            let payload = b"payload".to_vec();
            let (send, receive, buffer) = transfer(
                PayloadSenderConfig {
                    version,
                    certificate,
                    ..PayloadSenderConfig::default()
                },
                PayloadReceiverConfig {
                    version,
                    certificate,
                    ..PayloadReceiverConfig::default()
                },
                payload.clone(),
                payload.len() as u64,
            )
            .await;
            let send = send.unwrap();
            let receive = receive.unwrap();
            let expected = match version {
                TlsVersion::Tls12 => "TLSv1.2",
                TlsVersion::Tls13 => "TLSv1.3",
                TlsVersion::Both => unreachable!(),
            };
            assert_eq!(send.negotiated, expected);
            assert_eq!(receive.negotiated, expected);
            assert_eq!(buffer, payload);
        }
    }
}

#[tokio::test]
async fn truncate_sends_fewer_bytes_than_declared() {
    let declared = 1024u64;
    let payload = vec![7u8; declared as usize];
    let (send, receive, buffer) = transfer(
        PayloadSenderConfig {
            truncate_after: Some(declared - 1),
            ..PayloadSenderConfig::default()
        },
        PayloadReceiverConfig::default(),
        payload,
        declared,
    )
    .await;
    let send = send.unwrap();
    let receive = receive.unwrap();
    assert_eq!(send.bytes, declared - 1);
    assert_eq!(receive.bytes, declared - 1);
    assert!(receive.eof);
    assert_eq!(buffer.len(), (declared - 1) as usize);
    assert!(receive.bytes < declared);
}

#[tokio::test]
async fn extra_sends_more_bytes_than_declared() {
    let declared = 256usize;
    let extra = 16u64;
    let payload = vec![9u8; declared];
    let (send, receive, buffer) = transfer(
        PayloadSenderConfig {
            extra_bytes: extra,
            ..PayloadSenderConfig::default()
        },
        PayloadReceiverConfig::default(),
        payload.clone(),
        declared as u64 + extra,
    )
    .await;
    let send = send.unwrap();
    let receive = receive.unwrap();
    assert_eq!(send.bytes, declared as u64);
    assert_eq!(send.extra_bytes, extra);
    assert_eq!(receive.bytes, declared as u64 + extra);
    assert!(receive.eof);
    assert_eq!(&buffer[..declared], payload.as_slice());
    assert!(buffer[declared..].iter().all(|byte| *byte == 0));
}

#[tokio::test]
async fn extra_beyond_max_size_is_rejected() {
    let declared = 256usize;
    let (send, receive, _) = transfer(
        PayloadSenderConfig {
            extra_bytes: 16,
            ..PayloadSenderConfig::default()
        },
        PayloadReceiverConfig::default(),
        vec![9u8; declared],
        declared as u64,
    )
    .await;
    let _ = send;
    match receive {
        Err(TestkitError::TooLarge(limit)) => assert_eq!(limit, declared as u64),
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[tokio::test]
async fn a_wrong_sender_certificate_fails_the_receiver_pin() {
    let correct = PayloadSender::new(PayloadSenderConfig::default()).unwrap();
    let correct_cert = correct.cert_der().to_vec();
    drop(correct);
    let wrong = PayloadSender::new(PayloadSenderConfig {
        wrong_certificate: true,
        ..PayloadSenderConfig::default()
    })
    .unwrap();
    assert_ne!(wrong.cert_der(), correct_cert.as_slice());
    let receiver = PayloadReceiver::new(PayloadReceiverConfig {
        expected_peer_certificate: Some(correct_cert),
        ..PayloadReceiverConfig::default()
    })
    .unwrap();
    let payload = b"wrong certificate".to_vec();
    let mut buffer = Vec::new();
    let (_send, receive) = tokio::join!(
        wrong.accept_and_send(&payload),
        receiver.receive(address(&wrong), payload.len() as u64, &mut buffer),
    );
    assert!(
        matches!(receive, Err(TestkitError::PeerCertificate)),
        "got {receive:?}"
    );
}

#[tokio::test]
async fn a_wrong_receiver_certificate_fails_the_sender_pin() {
    let correct = PayloadReceiver::new(PayloadReceiverConfig::default()).unwrap();
    let correct_cert = correct.cert_der().unwrap().to_vec();
    drop(correct);
    let wrong = PayloadReceiver::new(PayloadReceiverConfig {
        wrong_certificate: true,
        ..PayloadReceiverConfig::default()
    })
    .unwrap();
    assert_ne!(wrong.cert_der().unwrap(), correct_cert.as_slice());
    let sender = PayloadSender::new(PayloadSenderConfig {
        expected_peer_certificate: Some(correct_cert),
        ..PayloadSenderConfig::default()
    })
    .unwrap();
    let payload = b"wrong certificate".to_vec();
    let mut buffer = Vec::new();
    let (send, _receive) = tokio::join!(
        sender.accept_and_send(&payload),
        wrong.receive(address(&sender), payload.len() as u64, &mut buffer),
    );
    assert!(
        matches!(send, Err(TestkitError::PeerCertificate)),
        "got {send:?}"
    );
}

#[tokio::test]
async fn a_wrong_rsa_sender_certificate_fails_the_receiver_pin() {
    let correct = PayloadSender::new(PayloadSenderConfig {
        certificate: CertificateKind::Rsa,
        ..PayloadSenderConfig::default()
    })
    .unwrap();
    let correct_cert = correct.cert_der().to_vec();
    drop(correct);
    let wrong = PayloadSender::new(PayloadSenderConfig {
        certificate: CertificateKind::Rsa,
        wrong_certificate: true,
        ..PayloadSenderConfig::default()
    })
    .unwrap();
    assert_ne!(wrong.cert_der(), correct_cert.as_slice());
    let receiver = PayloadReceiver::new(PayloadReceiverConfig {
        expected_peer_certificate: Some(correct_cert),
        ..PayloadReceiverConfig::default()
    })
    .unwrap();
    let payload = b"wrong rsa certificate".to_vec();
    let mut buffer = Vec::new();
    let (_send, receive) = tokio::join!(
        wrong.accept_and_send(&payload),
        receiver.receive(address(&wrong), payload.len() as u64, &mut buffer),
    );
    assert!(
        matches!(receive, Err(TestkitError::PeerCertificate)),
        "got {receive:?}"
    );
}

#[tokio::test]
async fn a_wrong_rsa_receiver_certificate_fails_the_sender_pin() {
    let correct = PayloadReceiver::new(PayloadReceiverConfig {
        certificate: CertificateKind::Rsa,
        ..PayloadReceiverConfig::default()
    })
    .unwrap();
    let correct_cert = correct.cert_der().unwrap().to_vec();
    drop(correct);
    let wrong = PayloadReceiver::new(PayloadReceiverConfig {
        certificate: CertificateKind::Rsa,
        wrong_certificate: true,
        ..PayloadReceiverConfig::default()
    })
    .unwrap();
    assert_ne!(wrong.cert_der().unwrap(), correct_cert.as_slice());
    let sender = PayloadSender::new(PayloadSenderConfig {
        certificate: CertificateKind::Rsa,
        expected_peer_certificate: Some(correct_cert),
        ..PayloadSenderConfig::default()
    })
    .unwrap();
    let payload = b"wrong rsa certificate".to_vec();
    let mut buffer = Vec::new();
    let (send, _receive) = tokio::join!(
        sender.accept_and_send(&payload),
        wrong.receive(address(&sender), payload.len() as u64, &mut buffer),
    );
    assert!(
        matches!(send, Err(TestkitError::PeerCertificate)),
        "got {send:?}"
    );
}

#[tokio::test]
async fn a_receiver_without_a_client_certificate_is_rejected() {
    let sender = PayloadSender::new(PayloadSenderConfig {
        idle_timeout: Duration::from_secs(2),
        ..PayloadSenderConfig::default()
    })
    .unwrap();
    let receiver = PayloadReceiver::new(PayloadReceiverConfig {
        present_client_certificate: false,
        read_timeout: Duration::from_secs(2),
        ..PayloadReceiverConfig::default()
    })
    .unwrap();
    let payload = b"no client certificate".to_vec();
    let mut buffer = Vec::new();
    let (send, receive) = tokio::join!(
        sender.accept_and_send(&payload),
        receiver.receive(address(&sender), payload.len() as u64, &mut buffer),
    );
    assert!(
        send.is_err(),
        "sender accepted a receiver without a certificate"
    );
    assert!(receive.is_err(), "receiver completed without a certificate");
    assert!(
        !matches!(send, Err(TestkitError::Timeout)),
        "the handshake must fail, not time out: {send:?}"
    );
}

#[tokio::test]
async fn a_raw_tcp_receiver_cannot_complete_the_tls_handshake() {
    let sender = PayloadSender::new(PayloadSenderConfig {
        idle_timeout: Duration::from_millis(300),
        ..PayloadSenderConfig::default()
    })
    .unwrap();
    let receiver = PayloadReceiver::new(PayloadReceiverConfig {
        raw_tcp: true,
        read_timeout: Duration::from_secs(2),
        ..PayloadReceiverConfig::default()
    })
    .unwrap();
    let payload = b"raw tcp".to_vec();
    let mut buffer = Vec::new();
    let (send, receive) = tokio::join!(
        sender.accept_and_send(&payload),
        receiver.receive(address(&sender), 1024, &mut buffer),
    );
    assert!(matches!(send, Err(TestkitError::Timeout)), "got {send:?}");
    let receive = receive.unwrap();
    assert_eq!(receive.negotiated, "raw");
    assert_eq!(receive.bytes, 0);
    assert!(receive.eof);
}

#[tokio::test]
async fn accept_timeout_fires_when_no_receiver_connects() {
    let sender = PayloadSender::new(PayloadSenderConfig {
        accept_timeout: Duration::from_millis(200),
        ..PayloadSenderConfig::default()
    })
    .unwrap();
    let started = Instant::now();
    let error = sender.accept_and_send(b"nobody").await.unwrap_err();
    let elapsed = started.elapsed();
    assert!(matches!(error, TestkitError::Timeout), "got {error:?}");
    assert!(
        elapsed >= Duration::from_millis(150),
        "the accept timeout fired before it waited: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the accept timeout fired late: {elapsed:?}"
    );
}

#[tokio::test]
async fn a_stalled_receiver_triggers_the_sender_idle_timeout() {
    let payload = vec![3u8; 8 * 1024 * 1024];
    let sender = PayloadSender::new(PayloadSenderConfig {
        idle_timeout: Duration::from_millis(300),
        ..PayloadSenderConfig::default()
    })
    .unwrap();
    let receiver = PayloadReceiver::new(PayloadReceiverConfig {
        stall_before_read: Some(Duration::from_secs(1)),
        read_timeout: Duration::from_secs(5),
        ..PayloadReceiverConfig::default()
    })
    .unwrap();
    let mut buffer = Vec::new();
    let started = Instant::now();
    let (send, receive) = tokio::join!(
        sender.accept_and_send(&payload),
        receiver.receive(address(&sender), payload.len() as u64, &mut buffer),
    );
    let elapsed = started.elapsed();
    assert!(matches!(send, Err(TestkitError::Timeout)), "got {send:?}");
    assert!(elapsed < Duration::from_secs(3), "sender took {elapsed:?}");
    match receive {
        Ok(report) => assert!(report.bytes < payload.len() as u64),
        Err(error) => assert!(
            !matches!(error, TestkitError::Timeout),
            "receiver timed out instead of seeing the sender close: {error:?}"
        ),
    }
}

#[tokio::test]
async fn pause_second_until_first_completes_delays_the_second_payload() {
    let first_payload = vec![1u8; 8 * 1024 * 1024];
    let second_payload = b"second".to_vec();
    let first_len = first_payload.len() as u64;
    let second_len = second_payload.len() as u64;
    let sender = Arc::new(
        PayloadSender::new(PayloadSenderConfig {
            pause_second_until_first_completes: true,
            idle_timeout: Duration::from_secs(5),
            accept_timeout: Duration::from_secs(5),
            ..PayloadSenderConfig::default()
        })
        .unwrap(),
    );
    let address = address(&sender);
    let first_receiver = PayloadReceiver::new(PayloadReceiverConfig {
        stall_before_read: Some(Duration::from_secs(1)),
        read_timeout: Duration::from_secs(5),
        ..PayloadReceiverConfig::default()
    })
    .unwrap();
    let second_receiver = PayloadReceiver::new(PayloadReceiverConfig::default()).unwrap();

    let first_started = Instant::now();
    let first_send = tokio::spawn({
        let sender = Arc::clone(&sender);
        let payload = first_payload.clone();
        async move {
            let result = sender.accept_and_send(&payload).await;
            (result, Instant::now())
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let first_receive = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let result = first_receiver
            .receive(address, first_len, &mut buffer)
            .await;
        (result, buffer)
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let second_send = tokio::spawn({
        let sender = Arc::clone(&sender);
        let payload = second_payload.clone();
        async move { sender.accept_and_send(&payload).await }
    });
    let second_receive = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let result = second_receiver
            .receive(address, second_len, &mut buffer)
            .await;
        (result, Instant::now(), buffer)
    });

    let (first_send_result, first_send_done) = first_send.await.unwrap();
    let (first_receive_result, first_buffer) = first_receive.await.unwrap();
    let second_send_result = second_send.await.unwrap();
    let (second_receive_result, second_receive_done, second_buffer) = second_receive.await.unwrap();

    assert_eq!(first_send_result.unwrap().bytes, first_len);
    assert_eq!(first_receive_result.unwrap().bytes, first_len);
    assert_eq!(first_buffer, first_payload);
    assert_eq!(second_send_result.unwrap().bytes, second_len);
    assert_eq!(second_receive_result.unwrap().bytes, second_len);
    assert_eq!(second_buffer, second_payload);
    assert!(
        first_send_done.duration_since(first_started) >= Duration::from_millis(900),
        "the first payload finished too early to exercise the pause: {:?}",
        first_send_done.duration_since(first_started)
    );
    assert!(
        second_receive_done > first_send_done,
        "the second payload started before the first completed"
    );
}

#[tokio::test]
async fn the_eight_second_peer_timeout_fires_before_the_ten_second_receiver_timeout() {
    let payload = vec![4u8; 8 * 1024 * 1024];
    let sender = PayloadSender::new(PayloadSenderConfig {
        idle_timeout: Duration::from_secs(8),
        ..PayloadSenderConfig::default()
    })
    .unwrap();
    let receiver = PayloadReceiver::new(PayloadReceiverConfig {
        stall_before_read: Some(Duration::from_secs(9)),
        read_timeout: Duration::from_secs(10),
        ..PayloadReceiverConfig::default()
    })
    .unwrap();
    let address = address(&sender);
    let mut buffer = Vec::new();

    let sender_started = Instant::now();
    let sender_future = async {
        let result = sender.accept_and_send(&payload).await;
        (result, sender_started.elapsed())
    };
    let receiver_started = Instant::now();
    let receiver_future = async {
        let result = receiver
            .receive(address, payload.len() as u64, &mut buffer)
            .await;
        (result, receiver_started.elapsed())
    };
    let ((send, send_elapsed), (receive, receive_elapsed)) =
        tokio::join!(sender_future, receiver_future);

    assert!(matches!(send, Err(TestkitError::Timeout)), "got {send:?}");
    assert!(
        send_elapsed >= Duration::from_secs(7),
        "the peer timeout fired early: {send_elapsed:?}"
    );
    assert!(
        send_elapsed < Duration::from_secs(10),
        "the peer timeout fired after the receiver deadline: {send_elapsed:?}"
    );
    assert!(
        receive_elapsed >= Duration::from_secs(9),
        "the receiver did not wait through the pre-read stall: {receive_elapsed:?}"
    );
    assert!(
        receive_elapsed < Duration::from_secs(10),
        "the receiver's timeout fired before the peer closed: {receive_elapsed:?}"
    );
    match receive {
        Ok(report) => assert!(report.bytes < payload.len() as u64),
        Err(error) => assert!(
            !matches!(error, TestkitError::Timeout),
            "the receiver's 10 s timeout fired first: {error:?}"
        ),
    }
}

#[tokio::test]
async fn the_receiver_read_timeout_covers_the_pre_read_stall() {
    let sender = PayloadSender::new(PayloadSenderConfig {
        stall_before_write: Some(Duration::from_secs(5)),
        idle_timeout: Duration::from_secs(5),
        ..PayloadSenderConfig::default()
    })
    .unwrap();
    let address = address(&sender);
    let sender_task = tokio::spawn(async move { sender.accept_and_send(b"stalled").await });
    let receiver = PayloadReceiver::new(PayloadReceiverConfig {
        stall_before_read: Some(Duration::from_millis(2000)),
        read_timeout: Duration::from_millis(1000),
        ..PayloadReceiverConfig::default()
    })
    .unwrap();
    let mut buffer = Vec::new();
    let started = Instant::now();
    let receive = receiver.receive(address, 7, &mut buffer).await;
    let elapsed = started.elapsed();
    sender_task.abort();

    assert!(
        matches!(receive, Err(TestkitError::Timeout)),
        "got {receive:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(900),
        "the read timeout fired before it waited: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1900),
        "the read timeout did not cover the stall: {elapsed:?}"
    );
}

#[tokio::test]
async fn payload_self_test_covers_sizes_versions_and_certificates() {
    let report = run_payload_self_test().await.unwrap();
    assert_eq!(report.cases.len(), 16);
    for case in &report.cases {
        assert!(case.eof);
        assert_eq!(case.bytes_sent, case.size);
        assert_eq!(case.bytes_received, case.size);
        match case.version {
            TlsVersion::Tls12 => assert_eq!(case.negotiated, "TLSv1.2"),
            TlsVersion::Tls13 => assert_eq!(case.negotiated, "TLSv1.3"),
            TlsVersion::Both => panic!("self-test must pin a TLS version"),
        }
    }
    for size in [0u64, 1, 1024 * 1024, 8 * 1024 * 1024] {
        assert!(report.cases.iter().any(|case| case.size == size));
    }
    for certificate in [CertificateKind::Ecdsa, CertificateKind::Rsa] {
        assert!(report
            .cases
            .iter()
            .any(|case| case.certificate == certificate));
    }
}

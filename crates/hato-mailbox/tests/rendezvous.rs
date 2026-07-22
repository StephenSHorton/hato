//! End-to-end tests for the mailbox: spin the axum server up in-process on a
//! loopback ephemeral port and drive it with the real `hato-code` client through
//! the full allocate → open → pake → verify → ticket handshake.

use std::net::SocketAddr;
use std::time::Duration;

use hato_code::mailbox::{Mailbox, Phase, Side};
use hato_mailbox::Config;
use tokio::sync::oneshot;

/// Bind an ephemeral loopback port, start the server with `config`, and return
/// its `ws://…/v1/ws` URL.
async fn start_server(config: Config) -> String {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        hato_mailbox::serve_with(listener, config).await.unwrap();
    });
    format!("ws://{addr}/v1/ws")
}

#[tokio::test]
async fn send_recv_roundtrip_recovers_exact_bytes() {
    let url = start_server(Config::default()).await;

    // Arbitrary opaque payload standing in for a BlobTicket's bytes.
    let payload: Vec<u8> = (0..300u32).map(|i| (i * 7 % 251) as u8).collect();

    let (code_tx, code_rx) = oneshot::channel::<String>();

    // Sender task: prints the code via the callback, then serves.
    let send_payload = payload.clone();
    let send_url = url.clone();
    let sender = tokio::spawn(async move {
        let mut code_tx = Some(code_tx);
        hato_code::send_ticket(
            &send_url,
            2,
            &send_payload,
            false,
            |code| {
                let _ = code_tx.take().unwrap().send(code.to_string());
            },
            |_sas| {},
        )
        .await
    });

    let code = code_rx.await.expect("sender produced a code");
    assert!(code.contains('-'), "code should be nameplate-words: {code}");

    let got = hato_code::recv_ticket(&url, &code, false, |_sas| {})
        .await
        .expect("receiver recovers the ticket");

    assert_eq!(got, payload, "receiver must recover the exact input bytes");
    sender.await.unwrap().expect("sender completes cleanly");
}

#[tokio::test]
async fn pair_exchanges_payloads_both_ways() {
    let url = start_server(Config::default()).await;
    let (code_tx, code_rx) = oneshot::channel::<String>();

    let host_payload = b"host-identity-json".to_vec();
    let join_payload = b"join-identity-json".to_vec();

    let host_url = url.clone();
    let host_payload_c = host_payload.clone();
    let host = tokio::spawn(async move {
        let mut code_tx = Some(code_tx);
        hato_code::pair_host(
            &host_url,
            2,
            &host_payload_c,
            false,
            |code| {
                let _ = code_tx.take().unwrap().send(code.to_string());
            },
            |_sas| {},
        )
        .await
    });

    let code = code_rx.await.expect("host produced a code");
    let peer_for_join = hato_code::pair_join(&url, &code, &join_payload, false, |_sas| {})
        .await
        .expect("join recovers host payload");
    assert_eq!(peer_for_join, host_payload);

    let peer_for_host = host.await.unwrap().expect("host recovers join payload");
    assert_eq!(peer_for_host, join_payload);
}

#[tokio::test]
async fn sas_matches_on_both_ends() {
    let url = start_server(Config::default()).await;
    let (code_tx, code_rx) = oneshot::channel::<String>();
    let (sas_s_tx, sas_s_rx) = oneshot::channel::<String>();

    let send_url = url.clone();
    let sender = tokio::spawn(async move {
        let mut code_tx = Some(code_tx);
        let mut sas_tx = Some(sas_s_tx);
        hato_code::send_ticket(
            &send_url,
            3,
            b"ticket",
            false,
            |code| {
                let _ = code_tx.take().unwrap().send(code.to_string());
            },
            |sas| {
                let _ = sas_tx.take().unwrap().send(sas.to_string());
            },
        )
        .await
    });

    let code = code_rx.await.unwrap();
    let (sas_r_tx, sas_r_rx) = oneshot::channel::<String>();
    let recv = tokio::spawn(async move {
        let mut sas_tx = Some(sas_r_tx);
        hato_code::recv_ticket(&url, &code, false, |sas| {
            let _ = sas_tx.take().unwrap().send(sas.to_string());
        })
        .await
    });

    let sas_s = sas_s_rx.await.unwrap();
    let sas_r = sas_r_rx.await.unwrap();
    assert_eq!(sas_s, sas_r, "the verify-aloud SAS must agree on both ends");
    assert!(sas_s.contains('-'), "SAS should be two words: {sas_s}");
    recv.await.unwrap().unwrap();
    sender.await.unwrap().unwrap();
}

#[tokio::test]
async fn wrong_code_aborts_before_ticket() {
    let url = start_server(Config::default()).await;
    let (code_tx, code_rx) = oneshot::channel::<String>();

    let send_url = url.clone();
    let sender = tokio::spawn(async move {
        let mut code_tx = Some(code_tx);
        hato_code::send_ticket(
            &send_url,
            2,
            b"secret-ticket",
            false,
            |code| {
                let _ = code_tx.take().unwrap().send(code.to_string());
            },
            |_sas| {},
        )
        .await
    });

    let code = code_rx.await.unwrap();
    // Keep the (correct) nameplate but corrupt the words so the verifier can't
    // match. Pick a word pair guaranteed to differ from the real code.
    let nameplate = code.split('-').next().unwrap();
    let wrong_a = format!("{nameplate}-aardvark-adroitness");
    let wrong_b = format!("{nameplate}-absurd-adviser");
    let wrong = if code == wrong_a { wrong_b } else { wrong_a };

    let err = hato_code::recv_ticket(&url, &wrong, false, |_| {})
        .await
        .expect_err("a wrong code must abort");
    assert!(
        matches!(err, hato_code::Error::VerifierMismatch),
        "expected VerifierMismatch, got {err:?}"
    );

    // The sender must also refuse to deliver the ticket (it aborts too).
    let send_result = sender.await.unwrap();
    assert!(
        matches!(send_result, Err(hato_code::Error::VerifierMismatch)),
        "sender must abort on a mismatched verifier, got {send_result:?}"
    );
}

#[tokio::test]
async fn third_side_is_crowded() {
    let url = start_server(Config::default()).await;

    // First side allocates + opens.
    let mut host = Mailbox::connect(&url).await.unwrap();
    let id = host.allocate().await.unwrap();
    host.open(id, Side::S).await.unwrap();

    // Second side joins fine.
    let mut peer = Mailbox::connect(&url).await.unwrap();
    peer.open(id, Side::R).await.unwrap();

    // Third side is rejected as crowded.
    let mut intruder = Mailbox::connect(&url).await.unwrap();
    intruder.open(id, Side::R).await.unwrap();
    let err = intruder
        .recv_phase(Phase::Pake)
        .await
        .expect_err("third side must be crowded");
    assert!(
        matches!(err, hato_code::Error::Crowded),
        "expected Crowded, got {err:?}"
    );
}

#[tokio::test]
async fn ttl_expires_idle_slots() {
    let config = Config {
        slot_ttl: Duration::from_millis(150),
        sweep_interval: Duration::from_millis(50),
        ..Config::default()
    };
    let url = start_server(config).await;

    // Sender allocates + opens, then waits for a peer that never comes.
    let mut host = Mailbox::connect(&url).await.unwrap();
    let id = host.allocate().await.unwrap();
    host.open(id, Side::S).await.unwrap();
    host.add(Phase::Pake, b"waiting").await.unwrap();

    // The TTL sweeper should eventually evict the slot and signal `gone`.
    let err = tokio::time::timeout(Duration::from_secs(3), host.recv_phase(Phase::Pake))
        .await
        .expect("sweeper should fire well within the timeout")
        .expect_err("an expired slot must report gone");
    assert!(
        matches!(err, hato_code::Error::Gone),
        "expected Gone after TTL, got {err:?}"
    );
}

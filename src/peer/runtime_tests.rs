use super::*;
use task_manager_peer::iroh::SecretKey;

#[tokio::test]
async fn the_link_binds_once_the_address_frees_up() {
    // A port that is taken: what a missing Wi-Fi address looks like to the bind.
    let held = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let bind = held.local_addr().unwrap();
    let report = Arc::new(Mutex::new(PeersReport::default()));
    let log: Logger = Arc::new(|_| {});
    let task = tokio::spawn({
        let (report, log) = (Arc::clone(&report), log.clone());
        async move {
            let key = SecretKey::generate();
            bind_when_possible(&key, bind, Duration::from_millis(50), &report, &log).await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !task.is_finished(),
        "must keep waiting while the port is taken"
    );
    let waiting = report.lock().unwrap().error.clone().unwrap_or_default();
    assert!(waiting.contains("waiting to bind"), "{waiting}");
    drop(held);
    let ep = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("binds after the port is free")
        .unwrap();
    assert_eq!(report.lock().unwrap().error, None);
    drop(ep);
}

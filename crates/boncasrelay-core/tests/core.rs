use boncasrelay_core::*;
#[test]
fn cpp_card_router_cases() {
    for a in [
        vec![0x90, 0x30, 0, 0, 0],
        vec![0x90, 0x34, 0, 0, 3, 1, 2, 3, 0],
        vec![0x90, 0x36, 0, 0, 1, 1, 0],
        vec![0x90, 0x32, 0, 0, 0],
        vec![],
        vec![0x90],
        vec![0, 0xa4, 4, 1],
    ] {
        assert_eq!(classify(&a), Kind::Bcas);
    }
    for a in [
        vec![0x90, 0xa0, 0, 1, 0x10],
        vec![0x90, 0x34, 0, 1, 3, 1, 2, 3, 0],
        vec![0x90, 0x30, 0, 1, 0],
    ] {
        assert_eq!(classify(&a), Kind::Acas);
    }
    assert!(is_auth(&[0x90, 0xa0]));
    assert!(!is_auth(&[0x90, 0x34, 0, 1]));
    let probe = auth_probe([1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(
        probe,
        [0x90, 0xa0, 0, 1, 0x10, 0, 0, 0, 1, 0, 0, 0x8a, 0xf7, 1, 2, 3, 4, 5, 6, 7, 8, 0]
    );
    assert_eq!(classify(&probe), Kind::Acas);
    assert_eq!(INIT_PROBE, [0x90, 0x30, 0, 0, 0]);
    assert_eq!(classify(&INIT_PROBE), Kind::Bcas);
    let mut acas = vec![0; 48];
    acas[46] = 0x90;
    assert!(acas_response(&acas));
    let error = [0, 4, 0, 0, 0xa1, 0xfe, 0x90, 0];
    assert!(!acas_response(&error));
    assert!(!bcas_response(&error));
    let mut init = vec![0; 59];
    init[57] = 0x90;
    init[7] = 5;
    assert!(bcas_response(&init));
    assert_eq!(ca_system_id(&init), 5);
    let mut ccas = init.clone();
    ccas[7] = 2;
    assert!(!bcas_response(&ccas));
    assert_eq!(ca_system_id(&ccas), 2);
    let mut fail = init;
    fail[57] = 0x6d;
    assert!(!bcas_response(&fail));
    let unavailable = Probe::default();
    let b = Probe {
        openable: true,
        bcas: true,
        acas: false,
    };
    let a = Probe {
        openable: true,
        bcas: false,
        acas: true,
    };
    let unknown = Probe {
        openable: true,
        ..Probe::default()
    };
    let rs = [unavailable, b, a, unknown];
    assert_eq!(select_reader(Kind::Bcas, &rs, None), Some(1));
    assert_eq!(select_reader(Kind::Acas, &rs, None), Some(2));
    assert_eq!(select_reader(Kind::Acas, &rs, Some(0)), Some(0));
    assert_eq!(select_reader(Kind::Bcas, &[b], None), Some(0));
    assert_eq!(select_reader(Kind::Acas, &[b], None), None);
    assert_eq!(select_reader(Kind::Acas, &[b, unknown], None), Some(1));
    for k in [Kind::Bcas, Kind::Acas] {
        assert_eq!(select_reader(k, &[unknown], None), Some(0));
        assert_eq!(select_reader(k, &[], None), None);
    }
    for n in 0..=255 {
        let a = vec![0; n];
        assert!(!success(&a));
        assert!(!acas_response(&a));
        assert!(!bcas_response(&a));
        assert_eq!(ca_system_id(&a), 0);
    }
}
#[test]
fn cidr_and_settings() {
    use config::{cidr, Config};
    for (net, yes, no) in [
        ("127.0.0.0/8", "127.1.2.3", "128.0.0.1"),
        ("2001:db8::/32", "2001:db8::ff", "2001:db9::"),
        ("::/0", "::1", "127.0.0.1"),
        ("127.0.0.1", "127.0.0.1", "127.0.0.2"),
    ] {
        let n = cidr(net).unwrap();
        assert!(n.contains(&yes.parse::<std::net::IpAddr>().unwrap()));
        assert!(!n.contains(&no.parse::<std::net::IpAddr>().unwrap()));
    }
    for bad in [
        "garbage",
        "::1/129",
        "127.0.0.1/33",
        "127.0.0.1/",
        "127.0.0.1/-1",
    ] {
        assert!(cidr(bad).is_err());
    }
    let path = std::env::temp_dir().join(format!("boncas-settings-{}.ini", std::process::id()));
    std::fs::write(&path,"\u{feff}[General]\nServerPort=1234\nAllow=192.0.2.0/24\nVerbose=1\n[CardReader]\nBCAS=old\n[CardPort]\n1235=old\n").unwrap();
    let args = vec![
        "--port".into(),
        "2345".into(),
        "--config".into(),
        path.to_string_lossy().into_owned(),
        "--allow".into(),
        "::1/128".into(),
        "--allow".into(),
        "127.0.0.1".into(),
        "--reader-port".into(),
        "new=2346".into(),
    ];
    let c = Config::parse(&args).unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(c.port, 2345);
    assert_eq!(c.pins[0], "old");
    assert_eq!(c.fixed.len(), 1);
    assert_eq!(c.fixed[&2346], "new");
    assert_eq!(c.allow.len(), 2);
    assert!(c.verbose);
    for bad in [
        vec!["--port", "+1"],
        vec!["--port", "0"],
        vec!["--reader-port", "x=6900"],
        vec!["--reader-port", "x=1", "--reader-port", "y=1"],
        vec!["--missing"],
    ] {
        assert!(Config::parse(&bad.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
    }
    let mut c = Config::default();
    assert!(c.apply_ini("[General]\nUnknown=1").is_err());
    assert!(c.apply_ini("[General]\nVerbose=yes").is_err());
}
#[test]
fn framing_fragmentation_pipeline_limits_and_deadline() {
    use std::{
        io::Write,
        net::{TcpListener, TcpStream},
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let child = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let stop = AtomicBool::new(false);
        assert_eq!(
            frame::read_frame(&mut s, Instant::now() + Duration::from_secs(2), &stop).unwrap(),
            vec![1, 2, 3]
        );
        assert!(
            frame::read_frame(&mut s, Instant::now() + Duration::from_secs(2), &stop)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            frame::read_frame(&mut s, Instant::now() + Duration::from_secs(2), &stop)
                .unwrap()
                .len(),
            255
        );
        assert!(frame::write_frame(
            &mut s,
            &[0; 256],
            Instant::now() + Duration::from_secs(1),
            &stop
        )
        .is_err());
        frame::write_frame(&mut s, &[], Instant::now() + Duration::from_secs(1), &stop).unwrap();
        let deadline = Instant::now() + Duration::from_millis(120);
        assert!(frame::read_frame(&mut s, deadline, &stop).is_err());
        assert!(deadline.elapsed() < Duration::from_millis(250));
    });
    let mut s = TcpStream::connect(addr).unwrap();
    for byte in [3, 1, 2, 3] {
        s.write_all(&[byte]).unwrap();
    }
    s.write_all(&[0, 255]).unwrap();
    s.write_all(&[0; 255]).unwrap();
    assert!(frame::read_frame(
        &mut s,
        Instant::now() + Duration::from_secs(2),
        &AtomicBool::new(false)
    )
    .unwrap()
    .is_empty());
    s.write_all(&[5, 1]).unwrap();
    child.join().unwrap();
}
#[test]
fn state_history_bounded_and_snapshot_independent() {
    let s = state::State::default();
    for _ in 0..300 {
        s.log("test");
    }
    let before = s.snapshot();
    assert_eq!(before.logs.len(), 256);
    s.update(|s| s.readers.push("reader".into()));
    assert!(before.readers.is_empty());
    assert_eq!(s.snapshot().readers.len(), 1);
}

#[test]
fn reader_selection_and_runtime_pin_replacement() {
    use backend::MockBackend;
    use router::{match_reader, Router};
    use std::sync::{atomic::AtomicBool, Arc};
    let names = vec!["Mock B-CAS".into(), "Mock ACAS".into(), "Mock C-CAS".into()];
    assert_eq!(
        match_reader(&names, "Mock ACAS").unwrap(),
        Some("Mock ACAS".into())
    );
    assert_eq!(
        match_reader(&names, "C-CAS").unwrap(),
        Some("Mock C-CAS".into())
    );
    assert!(match_reader(&names, "Mock").is_err());
    assert_eq!(match_reader(&names, "missing").unwrap(), None);
    let state = state::State::default();
    let backend = Arc::new(MockBackend::new(state.clone()));
    let router = Router::new(
        backend,
        config::Config::default(),
        state.clone(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let original = router.select(Kind::Bcas, "").unwrap();
    assert_eq!(original.name, "Mock B-CAS");
    assert_eq!(state.snapshot().routes[0], Some("Mock B-CAS".into()));
    // Changing pins must not replace the per-reader lock used by active clients.
    let guard = original.card.lock();
    router.replace_pins(["Mock C-CAS".into(), "Mock ACAS".into()]);
    assert!(state.snapshot().routes[0].is_none());
    drop(guard);
    let selected = router.select(Kind::Bcas, "").unwrap();
    assert_eq!(selected.name, "Mock C-CAS");
    let fixed = router.select(Kind::Bcas, "Mock C-CAS").unwrap();
    assert!(Arc::ptr_eq(&selected, &fixed));
    assert_eq!(
        state.snapshot().fixed_routes["Mock C-CAS"],
        Some("Mock C-CAS".into())
    );
}

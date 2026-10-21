use mahoquot_gateway::discovery::Browser as _;
use mahoquot_gateway::discovery::{
    build_service_spec, build_txt_records, filter_interfaces, parse_txt_records, DiscoveryConfig,
    DiscoveryManager, MockBrowser, MockMdnsRegistry, NetworkInterface, TxtOptions, DEFAULT_DOMAIN,
    DEFAULT_SERVICE_TYPE, PRODUCT_CPA,
};
use mahoquot_gateway::tui::{ManagementClient, PtySessionSimulator, Tab, TuiState};

#[test]
fn task34_debug_real_discovery() {
    let content = "discovery:\n  enabled: true\n  interfaces:\n    include: [lo0]\n";
    let val: serde_json::Value = serde_yaml::from_str(content).unwrap();
    let disc = val.get("discovery").unwrap();
    let cfg: DiscoveryConfig = serde_json::from_value(disc.clone()).unwrap();

    let ifaces = mahoquot_gateway::discovery::query_system_interfaces();
    let mut mgr = DiscoveryManager::real();
    let applied = mgr.apply_config(&cfg, Some("127.0.0.1"), 18840, false, &ifaces, None);
    assert!(
        applied,
        "discovery advertise failed: {:?}",
        mgr.last_error()
    );
    assert!(mgr.is_advertising());

    let browser = mahoquot_gateway::discovery::RealBrowser::new();
    let list = browser.browse("", "").unwrap();
    assert_eq!(list.len(), 1, "browse result: {list:?}");
    assert_eq!(list[0].port, 18840);
    assert_eq!(list[0].product, "cliproxyapi");
    mgr.shutdown();
    assert!(!mgr.is_advertising());
}

#[test]
fn task34_tui_pty_navigation_and_tab_flow() {
    let client = ManagementClient::new("http://127.0.0.1:18801".into(), "test-key".into());
    let state = TuiState::new(client, false, None);
    let mut sim = PtySessionSimulator::new(state);

    assert!(sim.state.authenticated);
    assert_eq!(sim.state.active_tab, Tab::Dashboard);
    assert!(sim.current_screen().contains("SCREEN: DASHBOARD"));

    sim.send_key("tab");
    assert_eq!(sim.state.active_tab, Tab::Config);
    assert!(sim.current_screen().contains("SCREEN: CONFIGURATION"));

    sim.send_key("tab");
    assert_eq!(sim.state.active_tab, Tab::Accounts);
    assert!(sim.current_screen().contains("SCREEN: ACCOUNTS LIST"));

    sim.send_key("tab");
    assert_eq!(sim.state.active_tab, Tab::Quota);
    assert!(sim.current_screen().contains("SCREEN: QUOTA & DIAGNOSTICS"));

    sim.send_key("tab");
    assert_eq!(sim.state.active_tab, Tab::Logs);
    assert!(sim.current_screen().contains("SCREEN: LOGS MONITOR"));

    sim.send_key("1");
    assert_eq!(sim.state.active_tab, Tab::Dashboard);

    let quit = sim.send_key("q");
    assert!(quit);
}

#[test]
fn task34_tui_unauth_flow() {
    let client = ManagementClient::new("http://127.0.0.1:18801".into(), "".into());
    let state = TuiState::new(client, false, None);
    let mut sim = PtySessionSimulator::new(state);

    assert!(!sim.state.authenticated);
    assert!(sim
        .current_screen()
        .contains("Mahoquot Gateway Management Login"));

    sim.send_key("enter");
    assert!(!sim.state.authenticated);
    assert_eq!(
        sim.state.auth_error.as_deref(),
        Some("Password is required")
    );

    sim.send_key("p");
    sim.send_key("a");
    sim.send_key("s");
    sim.send_key("s");
    sim.send_key("enter");

    assert!(sim.state.authenticated);
    assert_eq!(sim.state.client.base_url(), "http://127.0.0.1:18801");
}

#[test]
fn task34_discovery_txt_metadata_and_dns_sd_limits() {
    let opts = TxtOptions {
        instance_id: "A1B2".into(),
        advertise_management: true,
        tls: false,
        auth_required: true,
        ..Default::default()
    };

    let records = build_txt_records(&opts);
    let mut total_len = 0;
    for r in &records {
        assert!(r.len() <= 255);
        total_len += r.len() + 1;
    }
    assert!(total_len <= 400);

    let parsed = parse_txt_records(&records);
    assert_eq!(parsed.get("version").map(String::as_str), Some("1"));
    assert_eq!(parsed.get("product").map(String::as_str), Some(PRODUCT_CPA));
    assert_eq!(parsed.get("instance_id").map(String::as_str), Some("A1B2"));
    assert_eq!(parsed.get("management").map(String::as_str), Some("true"));
    assert_eq!(
        parsed.get("auth_required").map(String::as_str),
        Some("true")
    );
}

#[test]
fn task34_discovery_interface_filtering_and_spec_build() {
    let ifaces = vec![
        NetworkInterface {
            index: 1,
            name: "lo0".into(),
            is_up: true,
            is_loopback: true,
            is_multicast: true,
            is_point_to_point: false,
            addresses: vec!["127.0.0.1".parse().unwrap()],
        },
        NetworkInterface {
            index: 2,
            name: "en0".into(),
            is_up: true,
            is_loopback: false,
            is_multicast: true,
            is_point_to_point: false,
            addresses: vec!["192.168.1.120".parse().unwrap()],
        },
        NetworkInterface {
            index: 3,
            name: "tailscale0".into(),
            is_up: true,
            is_loopback: false,
            is_multicast: true,
            is_point_to_point: false,
            addresses: vec!["100.64.0.1".parse().unwrap()],
        },
    ];

    let filtered = filter_interfaces(&ifaces, &[], &[]);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].name, "en0");

    let cfg = DiscoveryConfig {
        enabled: true,
        service_name: "test-node".into(),
        ..Default::default()
    };

    let spec = build_service_spec(&cfg, None, 18801, false, &ifaces, None).unwrap();
    assert!(spec.instance_name.starts_with("test-node-"));
    assert_eq!(spec.port, 18801);
    assert_eq!(spec.advertised_ips, vec!["192.168.1.120"]);
}

#[test]
fn task34_discovery_manager_start_and_teardown() {
    let registry = MockMdnsRegistry::new();
    let mut mgr = DiscoveryManager::new(registry.clone());
    let browser = MockBrowser::new(registry.clone());

    let ifaces = vec![NetworkInterface {
        index: 1,
        name: "en0".into(),
        is_up: true,
        is_loopback: false,
        is_multicast: true,
        is_point_to_point: false,
        addresses: vec!["192.168.1.88".parse().unwrap()],
    }];

    let cfg = DiscoveryConfig {
        enabled: true,
        service_name: "cluster-gw".into(),
        ..Default::default()
    };

    assert!(mgr.apply_config(&cfg, None, 18801, false, &ifaces, None));
    assert!(mgr.is_advertising());

    let list = browser
        .browse(DEFAULT_SERVICE_TYPE, DEFAULT_DOMAIN)
        .unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].port, 18801);
    assert!(list[0].instance_name.starts_with("cluster-gw-"));

    mgr.shutdown();
    assert!(!mgr.is_advertising());

    let empty_list = browser
        .browse(DEFAULT_SERVICE_TYPE, DEFAULT_DOMAIN)
        .unwrap();
    assert_eq!(empty_list.len(), 0);
}

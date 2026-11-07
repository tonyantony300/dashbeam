//! Nearby / local-network discovery E2E.
//!
//! These bind real endpoints, so run with `--test-threads=1`.

mod common;

use common::TestNode;
use engine::{Discoverability, DiscoveryModeOption, PairingStatus};
use iroh::endpoint::{Connection, RelayMode};
use std::time::Duration;

const PRESENCE_DEADLINE: Duration = Duration::from_secs(60);
const NETWORK_READY_DEADLINE: Duration = Duration::from_secs(30);

/// Dials `to`'s control ALPN as an unpaired peer. Retried because the gate's
/// direct-path deadline can expire on a loaded runner.
async fn open_stranger_control_connection(from: &TestNode, to: &TestNode) -> Connection {
    let mut last = String::new();
    for _ in 0..5 {
        match from
            .open_control_connection_for_tests(&to.endpoint_id())
            .await
        {
            Ok(conn) => return conn,
            Err(err) => {
                last = format!("{err:#}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    panic!("could not open an unpaired control connection to a discoverable peer: {last}");
}

/// `node`'s stored pairing status for `endpoint_id`, if it has a record.
fn paired_status(node: &TestNode, endpoint_id: &str) -> Option<PairingStatus> {
    node.list_paired()
        .expect("list_paired")
        .into_iter()
        .find(|d| d.endpoint_id.eq_ignore_ascii_case(endpoint_id))
        .map(|d| d.pairing_status)
}

/// `node`'s view of whether `endpoint_id` is online.
fn is_online(node: &TestNode, endpoint_id: &str) -> bool {
    node.list_paired()
        .expect("list_paired")
        .into_iter()
        .find(|d| d.endpoint_id.eq_ignore_ascii_case(endpoint_id))
        .map(|d| d.online)
        .unwrap_or(false)
}

/// Polls `is_online` for `window`, failing on the first `false`. Presence flips
/// asynchronously, so a single point-in-time check can race past the failure.
async fn assert_stays_online(node: &TestNode, endpoint_id: &str, window: Duration) {
    let end = tokio::time::Instant::now() + window;
    while tokio::time::Instant::now() < end {
        assert!(
            is_online(node, endpoint_id),
            "presence flipped offline after the probe — the paired session was clobbered"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn unpaired_peer_can_probe_identity_when_discoverable_to_everyone() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Everyone).await;

    let identity = alice
        .probe_identity(&bob.endpoint_id())
        .await
        .expect("probe should succeed");

    assert_eq!(identity.endpoint_id, bob.endpoint_id());
    assert_eq!(identity.display_name, "bob");
}

/// A stranger probing in a tight loop exhausts its token bucket and gets
/// closed rather than served — see `native/src/rate_limit.rs`.
#[tokio::test]
async fn unpaired_probe_loop_is_rate_limited() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Everyone).await;

    // Well past the burst allowance; localhost probes outrun the refill.
    let mut denied = false;
    for _ in 0..20 {
        if alice.probe_identity(&bob.endpoint_id()).await.is_err() {
            denied = true;
            break;
        }
    }
    assert!(denied, "a probe loop must eventually be rate-limited");
}

#[tokio::test]
async fn unpaired_peer_is_refused_when_paired_only() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::PairedOnly).await;

    assert!(
        alice.probe_identity(&bob.endpoint_id()).await.is_err(),
        "an unpaired peer must not learn our name under PairedOnly"
    );
}

#[tokio::test]
async fn unpaired_peer_is_refused_when_off() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Off).await;

    assert!(alice.probe_identity(&bob.endpoint_id()).await.is_err());
}

#[tokio::test]
async fn already_paired_peers_still_probe_under_paired_only() {
    let (alice, bob) = common::spawn_paired_nodes().await;
    bob.set_discoverability(Discoverability::PairedOnly).await;

    // Baseline: bob's persistent connection is up before the probe.
    common::wait_until(
        "bob to see alice online before probing",
        PRESENCE_DEADLINE,
        || is_online(&bob, &alice.endpoint_id()),
    )
    .await;

    let identity = alice
        .probe_identity(&bob.endpoint_id())
        .await
        .expect("paired peers must still resolve under PairedOnly");
    assert_eq!(identity.display_name, "bob");

    assert_stays_online(&bob, &alice.endpoint_id(), Duration::from_secs(5)).await;
}

#[tokio::test]
async fn paired_peer_probe_refused_under_off_does_not_kill_session() {
    let (alice, bob) = common::spawn_paired_nodes().await;
    bob.set_discoverability(Discoverability::Off).await;

    common::wait_until(
        "bob to see alice online before probing",
        PRESENCE_DEADLINE,
        || is_online(&bob, &alice.endpoint_id()),
    )
    .await;

    assert!(
        alice.probe_identity(&bob.endpoint_id()).await.is_err(),
        "Off must refuse even an already-paired peer's probe"
    );

    assert!(
        is_online(&bob, &alice.endpoint_id()),
        "a refused probe must not tear down the paired peer's session"
    );
}

/// mDNS re-fires `Discovered` often, which maps to `nudge_reconnect` for
/// paired peers. Nudging must not abort a live session, or nearby-paired
/// devices flap online/offline.
#[tokio::test]
async fn mdns_rediscovery_does_not_flap_paired_presence() {
    let (alice, bob) = common::spawn_paired_nodes().await;

    common::wait_until(
        "bob to see alice online before rediscovery nudges",
        PRESENCE_DEADLINE,
        || is_online(&bob, &alice.endpoint_id()),
    )
    .await;
    common::wait_until(
        "alice to see bob online before rediscovery nudges",
        PRESENCE_DEADLINE,
        || is_online(&alice, &bob.endpoint_id()),
    )
    .await;

    // Mirrors a chatty mDNS republish on a busy LAN.
    for _ in 0..5 {
        bob.simulate_paired_lan_appeared_for_tests(&alice.endpoint_id())
            .await;
        alice
            .simulate_paired_lan_appeared_for_tests(&bob.endpoint_id())
            .await;
    }

    assert_stays_online(&bob, &alice.endpoint_id(), Duration::from_secs(5)).await;
    assert_stays_online(&alice, &bob.endpoint_id(), Duration::from_secs(5)).await;
}

/// A peer running the old abort-on-rediscovery behaviour must not clear our
/// view of them: our outbound is still alive.
#[tokio::test]
async fn peer_outbound_flaps_do_not_clear_our_presence() {
    let (alice, bob) = common::spawn_paired_nodes().await;

    common::wait_until("bob to see alice online", PRESENCE_DEADLINE, || {
        is_online(&bob, &alice.endpoint_id())
    })
    .await;
    common::wait_until("alice to see bob online", PRESENCE_DEADLINE, || {
        is_online(&alice, &bob.endpoint_id())
    })
    .await;

    // Alice is the unfixed peer: force-abort her outbound to bob repeatedly.
    for _ in 0..5 {
        alice
            .force_paired_reconnect_for_tests(&bob.endpoint_id())
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Bob is the device under test — his view of alice must stay online.
    assert_stays_online(&bob, &alice.endpoint_id(), Duration::from_secs(5)).await;
}

/// Real multicast. CI runners frequently block it, so this is opt-in:
/// `cargo test --manifest-path engine/Cargo.toml --test test_nearby -- --ignored --test-threads=1`
#[tokio::test]
#[ignore = "requires multicast on the local network"]
async fn two_nodes_discover_each_other_over_mdns() {
    let alice = common::spawn_node_with_lan_discovery("alice").await;
    let bob = common::spawn_node_with_lan_discovery("bob").await;

    let found = common::wait_for_nearby(
        &alice,
        &bob.endpoint_id(),
        std::time::Duration::from_secs(20),
    )
    .await
    .expect("bob should appear in alice's nearby list");

    assert_eq!(found.endpoint_id, bob.endpoint_id());
    assert!(found.identified, "probe should have resolved the name");
    assert_eq!(found.display_name.as_deref(), Some("bob"));
    assert_eq!(found.fingerprint.len(), 14);
}

/// `Discoverability` transitions across the `Off` boundary rebuild the
/// endpoint. Repeated to catch a stall or leak that only shows on a later cycle.
#[tokio::test]
async fn discoverability_off_then_on_rebuilds_the_endpoint_repeatedly() {
    let node = common::spawn_node("solo").await;
    common::wait_until("network ready after start", NETWORK_READY_DEADLINE, || {
        node.is_network_ready()
    })
    .await;

    for _ in 0..2 {
        node.set_discoverability(Discoverability::Off).await;
        assert_eq!(node.discoverability().await, Discoverability::Off);
        assert!(
            node.list_nearby().await.is_empty(),
            "moving to Off must clear the nearby list"
        );
        common::wait_until(
            "network ready after moving to Off",
            NETWORK_READY_DEADLINE,
            || node.is_network_ready(),
        )
        .await;

        node.set_discoverability(Discoverability::Everyone).await;
        assert_eq!(node.discoverability().await, Discoverability::Everyone);
        common::wait_until(
            "network ready after leaving Off",
            NETWORK_READY_DEADLINE,
            || node.is_network_ready(),
        )
        .await;
    }
}

/// `Everyone` <-> `PairedOnly` must not touch mDNS state at all — no rebuild,
/// no network-warming blip.
#[tokio::test]
async fn paired_only_transition_does_not_rebuild_the_network() {
    let node = common::spawn_node("solo").await;
    common::wait_until("network ready after start", NETWORK_READY_DEADLINE, || {
        node.is_network_ready()
    })
    .await;

    node.set_discoverability(Discoverability::PairedOnly).await;
    assert_eq!(node.discoverability().await, Discoverability::PairedOnly);
    // A rebuild would have flipped network_ready to false by now.
    assert!(node.is_network_ready());

    node.set_discoverability(Discoverability::Everyone).await;
    assert_eq!(node.discoverability().await, Discoverability::Everyone);
    assert!(node.is_network_ready());
}

/// `set_discoverability` and `reconfigure_network` both rebuild the network, so
/// `network_transition` must serialize them. Two concurrent calls have no
/// deterministic winner; the assertions only check the final state is
/// internally consistent and the node still usable.
#[tokio::test]
async fn concurrent_discoverability_toggle_and_reconfigure_settle_consistently() {
    let node = common::spawn_node("solo").await;
    common::wait_until("network ready after start", NETWORK_READY_DEADLINE, || {
        node.is_network_ready()
    })
    .await;

    let toggle = node.set_discoverability(Discoverability::Off);
    let reconfigure = node.reconfigure_network(RelayMode::Disabled, DiscoveryModeOption::Default);
    let (_, reconfigure_result) = tokio::join!(toggle, reconfigure);
    reconfigure_result
        .expect("reconfigure must not error just because it raced a discoverability change");

    common::wait_until(
        "network ready after the concurrent transitions settle",
        NETWORK_READY_DEADLINE,
        || node.is_network_ready(),
    )
    .await;

    // If discoverability landed on Off, the registry must actually be empty.
    if node.discoverability().await == Discoverability::Off {
        assert!(
            node.list_nearby().await.is_empty(),
            "Off must mean an empty nearby list, even after a race"
        );
    }

    // A wedged or doubly-built endpoint would hang or error here.
    node.reconfigure_network(RelayMode::Default, DiscoveryModeOption::Default)
        .await
        .expect("node must still be reconfigurable after the race");
    common::wait_until(
        "network ready after the follow-up reconfigure",
        NETWORK_READY_DEADLINE,
        || node.is_network_ready(),
    )
    .await;
}

/// Real multicast — opt-in, see `two_nodes_discover_each_other_over_mdns`.
///
/// A peer that goes `Off` must disappear from the *other* side's Nearby list,
/// not just clear its own — aborting the consumer task alone left the
/// registered `MdnsAddressLookup` advertising forever.
#[tokio::test]
#[ignore = "requires multicast on the local network"]
async fn discoverability_off_stops_mdns_advertising() {
    let alice = common::spawn_node_with_lan_discovery("alice").await;
    let bob = common::spawn_node_with_lan_discovery("bob").await;

    common::wait_for_nearby(
        &bob,
        &alice.endpoint_id(),
        std::time::Duration::from_secs(20),
    )
    .await
    .expect("bob should discover alice before the Off toggle");

    alice.set_discoverability(Discoverability::Off).await;

    let vanished = common::wait_until_absent(
        &bob,
        &alice.endpoint_id(),
        std::time::Duration::from_secs(60),
    )
    .await;
    assert!(
        vanished,
        "alice must actually stop advertising once Off — bob should stop seeing her, \
         not just have her own local list clear"
    );
}

// The tests below seed the Nearby list via `inject_nearby_device_for_tests`
// rather than real multicast, so they run on any CI runner. `NearbyRegistry`
// is pure state — only the socket is skipped.

#[tokio::test]
async fn accepting_a_nearby_invite_promotes_the_sender_to_paired() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Everyone).await;

    assert!(
        bob.list_paired().unwrap().is_empty(),
        "precondition: bob knows nobody"
    );

    alice
        .inject_nearby_device_for_tests(&bob.endpoint_id())
        .await;

    let delivered = alice
        .invite_nearby_device(&bob.endpoint_id(), "test-blob-ticket", 1, 12)
        .await
        .expect("invite should be delivered");
    assert!(delivered, "invite must report delivered");

    bob.accept_nearby_invite(&alice.endpoint_id())
        .await
        .expect("accept should succeed");

    let paired = bob.list_paired().unwrap();
    assert_eq!(paired.len(), 1, "accepting must create a paired record");
    assert_eq!(paired[0].endpoint_id, alice.endpoint_id());

    // The sender observes the acceptance via the same
    // `paired-invite-response` event. Delivery is fire-and-forget, so poll.
    common::wait_until(
        "alice to observe bob's acceptance",
        Duration::from_secs(15),
        || alice.events.has_event("paired-invite-response"),
    )
    .await;
    let responses = alice.events.events_with_name("paired-invite-response");
    assert_eq!(responses.len(), 1);
    let payload: serde_json::Value =
        serde_json::from_str(responses[0].payload.as_deref().unwrap()).unwrap();
    assert_eq!(payload["endpoint_id"], bob.endpoint_id());
    assert_eq!(payload["response"], "accepted");

    // Pairing must be mutual, or the responder's presence connection back to
    // the sender is rejected as an unpaired peer's.
    common::wait_until(
        "alice to also record bob as paired (mutual nearby pairing)",
        Duration::from_secs(15),
        || {
            alice
                .list_paired()
                .unwrap()
                .iter()
                .any(|d| d.endpoint_id == bob.endpoint_id())
        },
    )
    .await;
    let alice_paired = alice.list_paired().unwrap();
    assert_eq!(
        alice_paired.len(),
        1,
        "the sender must also record the responder as paired"
    );
    assert_eq!(alice_paired[0].endpoint_id, bob.endpoint_id());

    // A promoted device must not also appear under the sender's Nearby list.
    assert!(
        !alice
            .list_nearby()
            .await
            .iter()
            .any(|d| d.endpoint_id == bob.endpoint_id()),
        "a promoted device must not also appear under the sender's Nearby list"
    );

    // Presence must establish in both directions, not just the records exist.
    common::wait_until(
        "alice to see bob online after mutual nearby pairing",
        PRESENCE_DEADLINE,
        || is_online(&alice, &bob.endpoint_id()),
    )
    .await;
    common::wait_until(
        "bob to see alice online after mutual nearby pairing",
        PRESENCE_DEADLINE,
        || is_online(&bob, &alice.endpoint_id()),
    )
    .await;
    assert_stays_online(&alice, &bob.endpoint_id(), Duration::from_secs(3)).await;
    assert_stays_online(&bob, &alice.endpoint_id(), Duration::from_secs(3)).await;
}

#[tokio::test]
async fn declining_a_nearby_invite_leaves_the_sender_unpaired() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Everyone).await;

    alice
        .inject_nearby_device_for_tests(&bob.endpoint_id())
        .await;

    let delivered = alice
        .invite_nearby_device(&bob.endpoint_id(), "test-blob-ticket", 1, 4)
        .await
        .expect("invite should be delivered");
    assert!(delivered);

    bob.decline_nearby_invite(&alice.endpoint_id(), false)
        .await
        .expect("decline should succeed");

    assert!(
        bob.list_paired().unwrap().is_empty(),
        "declining must not pair"
    );

    // Sender-side decline, same event as an accept — polled for the same reason.
    common::wait_until(
        "alice to observe bob's decline",
        Duration::from_secs(15),
        || alice.events.has_event("paired-invite-response"),
    )
    .await;
    let responses = alice.events.events_with_name("paired-invite-response");
    assert_eq!(responses.len(), 1);
    let payload: serde_json::Value =
        serde_json::from_str(responses[0].payload.as_deref().unwrap()).unwrap();
    assert_eq!(payload["endpoint_id"], bob.endpoint_id());
    assert_eq!(payload["response"], "declined");

    // A decline stays toast-only: pairing only commits on an accept.
    assert!(
        alice.list_paired().unwrap().is_empty(),
        "declining must not pair the sender either"
    );
}

#[tokio::test]
async fn a_promoted_device_leaves_the_nearby_list() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Everyone).await;

    alice
        .inject_nearby_device_for_tests(&bob.endpoint_id())
        .await;

    alice
        .invite_nearby_device(&bob.endpoint_id(), "test-blob-ticket", 1, 1)
        .await
        .unwrap();
    bob.accept_nearby_invite(&alice.endpoint_id())
        .await
        .unwrap();

    assert!(
        !bob.list_nearby()
            .await
            .iter()
            .any(|d| d.endpoint_id == alice.endpoint_id()),
        "a paired device must not also appear under Nearby"
    );
}

/// The accept notification back to a vanished sender is best-effort — the
/// durable side effects already happened, so `accept_nearby_invite` must not
/// return `Err`.
#[tokio::test]
async fn accept_nearby_invite_succeeds_even_if_the_sender_is_unreachable() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Everyone).await;

    alice
        .inject_nearby_device_for_tests(&bob.endpoint_id())
        .await;

    alice
        .invite_nearby_device(&bob.endpoint_id(), "test-blob-ticket", 1, 5)
        .await
        .expect("invite should be delivered");

    // The sender vanishes before bob gets around to accepting.
    alice.shutdown().await.expect("alice shutdown");

    bob.accept_nearby_invite(&alice.endpoint_id())
        .await
        .expect("accept must succeed even though the sender can no longer be notified");

    let paired = bob.list_paired().unwrap();
    assert_eq!(paired.len(), 1, "the pairing must still be committed");
    assert_eq!(paired[0].endpoint_id, alice.endpoint_id());
}

/// An unpaired peer may send `InviteResponse`, but only one matching an invite
/// we actually sent may be acted on — otherwise a stranger could spoof an
/// acceptance.
#[tokio::test]
async fn unsolicited_invite_response_from_an_unpaired_peer_is_ignored() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    // The handshake gate must let bob in for the message-level check to apply.
    alice.set_discoverability(Discoverability::Everyone).await;

    // Bob never received an invite; `decline_nearby_invite` is just the entry
    // point that sends a raw `InviteResponse` unprompted.
    bob.decline_nearby_invite(&alice.endpoint_id(), false)
        .await
        .expect("sending the message itself must still succeed");

    // Give any incorrect processing a moment to happen.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !alice.events.has_event("paired-invite-response"),
        "alice never sent bob a nearby invite, so this response must be ignored"
    );
}

#[tokio::test]
async fn accepting_a_nearby_pair_request_promotes_both_sides() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Everyone).await;

    alice
        .inject_nearby_device_for_tests(&bob.endpoint_id())
        .await;

    let delivered = alice
        .request_nearby_pair(&bob.endpoint_id())
        .await
        .expect("pair request should be delivered");
    assert!(delivered, "pair request must report delivered");

    common::wait_until(
        "bob to observe alice's pair request",
        Duration::from_secs(15),
        || bob.events.has_event("nearby-pair-request-received"),
    )
    .await;
    let requests = bob.events.events_with_name("nearby-pair-request-received");
    assert_eq!(requests.len(), 1);
    let payload: serde_json::Value =
        serde_json::from_str(requests[0].payload.as_deref().unwrap()).unwrap();
    assert_eq!(payload["remote_endpoint_id"], alice.endpoint_id());
    assert_eq!(payload["sender_name"], "alice");

    bob.accept_nearby_invite(&alice.endpoint_id())
        .await
        .expect("accept should succeed");

    let paired = bob.list_paired().unwrap();
    assert_eq!(paired.len(), 1);
    assert_eq!(paired[0].endpoint_id, alice.endpoint_id());

    common::wait_until(
        "alice to observe bob's acceptance",
        Duration::from_secs(15),
        || alice.events.has_event("paired-invite-response"),
    )
    .await;

    common::wait_until(
        "alice to also record bob as paired",
        Duration::from_secs(15),
        || {
            alice
                .list_paired()
                .unwrap()
                .iter()
                .any(|d| d.endpoint_id == bob.endpoint_id())
        },
    )
    .await;
}

/// A `PairedOnly` requester (the agent's default) must still let the accept
/// back in. Regression: the handshake gate admitted unpaired peers only at
/// `Everyone`, so the ack was rejected and pairing stayed one-sided.
#[tokio::test]
async fn paired_only_requester_receives_the_accept() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    alice.set_discoverability(Discoverability::PairedOnly).await;
    bob.set_discoverability(Discoverability::Everyone).await;

    alice
        .inject_nearby_device_for_tests(&bob.endpoint_id())
        .await;
    assert!(
        alice
            .request_nearby_pair(&bob.endpoint_id())
            .await
            .expect("pair request should be delivered"),
        "pair request must report delivered"
    );
    common::wait_until(
        "bob to observe alice's pair request",
        Duration::from_secs(15),
        || bob.events.has_event("nearby-pair-request-received"),
    )
    .await;

    bob.accept_nearby_invite(&alice.endpoint_id())
        .await
        .expect("accept should succeed");

    common::wait_until(
        "alice to record bob as paired",
        Duration::from_secs(15),
        || paired_status(&alice, &bob.endpoint_id()).is_some(),
    )
    .await;
}

/// Accepting a nearby pair request must leave a durable pairing on BOTH sides.
///
/// Regression: refreshing `paired_connections` before delivering the accept
/// dialed a `Recognition` the uncommitted sender closed, read as a remote unpair.
#[tokio::test]
async fn accepting_a_nearby_pair_request_leaves_both_sides_active() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Everyone).await;

    alice
        .inject_nearby_device_for_tests(&bob.endpoint_id())
        .await;

    assert!(
        alice
            .request_nearby_pair(&bob.endpoint_id())
            .await
            .expect("pair request should be delivered"),
        "pair request must report delivered"
    );
    common::wait_until(
        "bob to observe alice's pair request",
        Duration::from_secs(15),
        || bob.events.has_event("nearby-pair-request-received"),
    )
    .await;

    bob.accept_nearby_invite(&alice.endpoint_id())
        .await
        .expect("accept should succeed");

    common::wait_until(
        "alice to record bob as paired",
        Duration::from_secs(15),
        || paired_status(&alice, &bob.endpoint_id()).is_some(),
    )
    .await;

    // The unpair would arrive asynchronously, so poll.
    for elapsed_ms in (250..=10_000).step_by(250) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            paired_status(&bob, &alice.endpoint_id()),
            Some(PairingStatus::Active),
            "after {elapsed_ms}ms bob's record for alice must still be Active"
        );
        assert_eq!(
            paired_status(&alice, &bob.endpoint_id()),
            Some(PairingStatus::Active),
            "after {elapsed_ms}ms alice's record for bob must still be Active"
        );
    }
}

/// The initiator must store the peer's real name once its probe lands.
///
/// Regression: persisting the click-time snapshot verbatim left an
/// `endpoint_id[..8]` prefix as the stored name when the probe was still in flight.
#[tokio::test]
async fn the_initiator_stores_the_peer_name_when_the_probe_lands_late() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Everyone).await;

    // Clicked Pair while the identity probe was still in flight.
    alice
        .inject_nearby_device_for_tests(&bob.endpoint_id())
        .await;
    assert!(alice
        .request_nearby_pair(&bob.endpoint_id())
        .await
        .expect("pair request should be delivered"));
    common::wait_until(
        "bob to observe alice's pair request",
        Duration::from_secs(15),
        || bob.events.has_event("nearby-pair-request-received"),
    )
    .await;

    // Probe lands before bob gets round to accepting.
    alice
        .inject_identified_nearby_device_for_tests(&bob.endpoint_id())
        .await
        .expect("identity probe should succeed");

    bob.accept_nearby_invite(&alice.endpoint_id())
        .await
        .expect("accept should succeed");
    common::wait_until(
        "alice to record bob as paired",
        Duration::from_secs(15),
        || paired_status(&alice, &bob.endpoint_id()).is_some(),
    )
    .await;

    let stored = alice
        .list_paired()
        .expect("list_paired")
        .into_iter()
        .find(|d| d.endpoint_id.eq_ignore_ascii_case(&bob.endpoint_id()))
        .expect("alice must have a record for bob");
    assert_eq!(
        stored.display_name, "bob",
        "alice stored a stale endpoint-id prefix instead of bob's display name"
    );
}

/// A declined nearby pair request must name the peer that declined.
///
/// Regression: the name was resolved from `paired_store` alone, and a decline
/// creates no record there — so the UI showed an endpoint-id prefix.
#[tokio::test]
async fn a_declined_nearby_pair_request_names_the_peer_that_declined() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Everyone).await;

    alice
        .inject_identified_nearby_device_for_tests(&bob.endpoint_id())
        .await
        .expect("identity probe should succeed");

    assert!(alice
        .request_nearby_pair(&bob.endpoint_id())
        .await
        .expect("pair request should be delivered"));
    common::wait_until(
        "bob to observe alice's pair request",
        Duration::from_secs(15),
        || bob.events.has_event("nearby-pair-request-received"),
    )
    .await;

    bob.decline_nearby_invite(&alice.endpoint_id(), false)
        .await
        .expect("decline should be delivered");

    common::wait_until(
        "alice to observe bob's decline",
        Duration::from_secs(15),
        || alice.events.has_event("paired-invite-response"),
    )
    .await;

    let events = alice.events.events_with_name("paired-invite-response");
    let payload: serde_json::Value =
        serde_json::from_str(events[0].payload.as_deref().expect("payload")).unwrap();
    assert_eq!(payload["response"], "declined");
    assert_eq!(
        payload["display_name"], "bob",
        "the decline must name bob, not an endpoint-id prefix"
    );
}

/// A pairing that commits while a control connection is already open must
/// apply to that connection too.
///
/// Regression: `handle_control_session` snapshotted paired-ness at accept time.
/// Nearby pairing opens two connections back to back, so the `Recognition` on
/// the second was rejected 403 by a session snapshotted before the commit —
/// which the dialer reads as a remote unpair.
#[tokio::test]
async fn a_pairing_that_commits_mid_session_is_not_rejected_as_unpaired() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Everyone).await;

    // Accepted while alice is still a stranger — the session that loses the race.
    let conn = open_stranger_control_connection(&alice, &bob).await;

    // Both halves commit after that connection was accepted.
    bob.remember_paired_device_for_tests(&alice.endpoint_id())
        .await
        .expect("bob commits the pairing");
    alice
        .remember_paired_device_for_tests(&bob.endpoint_id())
        .await
        .expect("alice commits the pairing");

    alice
        .send_recognition_for_tests(&conn)
        .await
        .expect("recognition should be written");

    // Pre-fix bob closes 403 within milliseconds; post-fix the session survives.
    let closed = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
    assert!(
        closed.is_err(),
        "bob must serve a Recognition from a peer it paired mid-session, got close: {closed:?}"
    );
}

/// The mid-session re-check must not become a way in for actual strangers:
/// a peer nobody paired still gets its `Recognition` refused.
#[tokio::test]
async fn a_strangers_recognition_is_still_rejected() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Everyone).await;

    let conn = open_stranger_control_connection(&alice, &bob).await;
    alice
        .send_recognition_for_tests(&conn)
        .await
        .expect("recognition should be written");

    let closed = tokio::time::timeout(Duration::from_secs(10), conn.closed())
        .await
        .expect("bob must close an unpaired peer's Recognition");
    assert!(
        format!("{closed}").contains("not permitted for unpaired peer"),
        "expected the unpaired-peer rejection, got: {closed}"
    );
}

/// A `Recognition` refused while a fresh pairing is still settling means the
/// peer has not committed its half yet — not that it unpaired us.
///
/// Regression: any 403 close was read as a remote unpair, so a peer committing
/// a beat late flipped our record to `UnpairedRemotely`.
#[tokio::test]
async fn a_recognition_refused_while_the_pairing_settles_does_not_unpair() {
    let alice = common::spawn_node("alice").await;
    let bob = common::spawn_node("bob").await;
    bob.set_discoverability(Discoverability::Everyone).await;

    // Alice commits and dials; bob hasn't, so he refuses her `Recognition`.
    alice
        .remember_paired_device_for_tests(&bob.endpoint_id())
        .await
        .expect("alice commits the pairing");

    // Long enough for alice's first dial to be refused — `has_direct_path`
    // alone holds a stranger's connection for three seconds.
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    bob.remember_paired_device_for_tests(&alice.endpoint_id())
        .await
        .expect("bob commits the pairing late");

    // Poll past the settling grace, so this also proves alice recovers into a
    // healthy pairing rather than merely deferring the damage.
    for elapsed_ms in (500..=15_000).step_by(500) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            paired_status(&alice, &bob.endpoint_id()),
            Some(PairingStatus::Active),
            "after {elapsed_ms}ms alice's record for bob must still be Active"
        );
    }
}
